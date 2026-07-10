#version 450

// Shadow material: a gaussian-blurred rounded rectangle (Evan Wallace's fast rounded-rect shadow),
// with an optional window-geometry cutout. Ported verbatim from render_helpers/shaders/shadow.frag
// + rounding_alpha.frag; the GLES named uniforms become the shared ShadowPush push constant, and
// niri_v_coords becomes v_uv (0..1). Outputs premultiplied color (premultiplied-over blend).

layout(push_constant) uniform Push {
    vec2 origin;
    vec2 size;
    vec4 proj; // unused here; declared so the push-block offsets match the vertex stage
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

layout(location = 0) in vec2 v_uv;
layout(location = 0) out vec4 o;

// A standard gaussian function, used for weighting samples.
float gaussian(float x, float sigma) {
    const float pi = 3.141592653589793;
    return exp(-(x * x) / (2.0 * sigma * sigma)) / (sqrt(2.0 * pi) * sigma);
}

// Approximates the error function, needed for the gaussian integral.
vec2 erf(vec2 x) {
    vec2 s = sign(x), a = abs(x);
    x = 1.0 + (0.278393 + (0.230389 + 0.078108 * (a * a)) * a) * a;
    x *= x;
    return s - s / (x * x);
}

// The blurred mask along the x dimension.
float roundedBoxShadowX(float x, float y, float sigma, float corner, vec2 halfSize) {
    float delta = min(halfSize.y - corner - abs(y), 0.0);
    float curved = halfSize.x - corner + sqrt(max(0.0, corner * corner - delta * delta));
    vec2 integral = 0.5 + 0.5 * erf((x + vec2(-curved, curved)) * (sqrt(0.5) / sigma));
    return integral.y - integral.x;
}

// The mask for the shadow of a box from lower to upper.
float roundedBoxShadow(vec2 lower, vec2 upper, vec2 point, float sigma, float corner) {
    vec2 center = (lower + upper) * 0.5;
    vec2 halfSize = (upper - lower) * 0.5;
    point -= center;

    float low = point.y - halfSize.y;
    float high = point.y + halfSize.y;
    float start = clamp(-3.0 * sigma, low, high);
    float end = clamp(3.0 * sigma, low, high);

    float step = (end - start) / 4.0;
    float y = start + step * 0.5;
    float value = 0.0;
    for (int i = 0; i < 4; i++) {
        value += roundedBoxShadowX(point.x, point.y - y, sigma, corner, halfSize) * gaussian(y, sigma) * step;
        y += step;
    }

    return value;
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
    vec2 coords_window_geo = v_uv * pc.area_size - pc.window_geo_loc;

    vec4 color = pc.shadow_color;

    float shadow_value;
    if (pc.sigma < 0.1) {
        // With low enough sigma just draw a rounded rectangle.
        shadow_value = niri_rounding_alpha(coords_geo, pc.geo_size, pc.corner_radius);
    } else {
        shadow_value = roundedBoxShadow(
            vec2(0.0, 0.0),
            pc.geo_size,
            coords_geo,
            pc.sigma,
            // FIXME (matches GLES): single corner radius for the blur.
            pc.corner_radius.x
        );
    }
    color = color * shadow_value;

    // Cut out the inside of the window geometry if requested.
    if (pc.window_geo_size != vec2(0.0, 0.0)) {
        if (0.0 <= coords_window_geo.x && coords_window_geo.x <= pc.window_geo_size.x
                && 0.0 <= coords_window_geo.y && coords_window_geo.y <= pc.window_geo_size.y) {
            float alpha = niri_rounding_alpha(coords_window_geo, pc.window_geo_size, pc.window_corner_radius);
            color = color * (1.0 - alpha);
        }
    }

    color = color * pc.niri_alpha;
    o = color;
}
