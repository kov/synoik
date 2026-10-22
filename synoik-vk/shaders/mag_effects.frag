// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

#version 450

// The magnifier's colour effects (gnome-shell's `MagShaderEffects`, `magnifier.js:2075-2174`) in
// one pass, in the order Clutter applies the three effects it stacks on the zoom region: the
// last added runs first on the content, so desaturate, then brightness/contrast, then invert
// lightness. Each Clutter effect writes an 8-bit framebuffer, so values clamp between stages.
//
// - `ClutterDesaturateEffect` (`clutter-desaturate-effect.c:70-80`).
// - `ClutterBrightnessContrastEffect` (`clutter-brightness-contrast-effect.c:66-77`), with its
//   multiplier, offset and contrast folded on the CPU into one premultiplied affine per channel:
//   rgb * scale + a * bias.
// - `ShellInvertLightnessEffect` (`shell-invert-lightness-effect.c:61-71`).

layout(set = 0, binding = 0) uniform sampler2D tex;

layout(push_constant) uniform Push {
    vec2 origin;
    vec2 size;
    vec4 proj; // unused here; declared so the push-block offsets match the vertex stage
    vec2 target;
    float corner_radius;
    float stroke_width;
    vec4 color;          // premultiplied tint, [alpha,alpha,alpha,alpha]
    vec4 st0;            // tex_transform columns (xyz used): v_uv -> normalized UV
    vec4 st1;
    vec4 st2;
    float desaturation;  // 0 = unchanged, 1 = grey
    float invert;        // > 0.5 inverts the lightness
    vec4 scale;          // brightness/contrast multiplier per channel (xyz)
    vec4 bias;           // ... and offset, times alpha (xyz)
} pc;

layout(location = 0) in vec2 v_uv;
layout(location = 1) in vec2 v_local;
layout(location = 0) out vec4 o;

void main() {
    vec2 uv = (mat3(pc.st0.xyz, pc.st1.xyz, pc.st2.xyz) * vec3(v_uv, 1.0)).xy;
    vec4 c = texture(tex, uv);

    vec3 gray = vec3(dot(vec3(0.299, 0.587, 0.114), c.rgb));
    c.rgb = mix(c.rgb, gray, pc.desaturation);

    c.rgb = clamp(c.rgb * pc.scale.xyz + c.a * pc.bias.xyz, 0.0, 1.0);

    if (pc.invert > 0.5) {
        float max_color = max(c.r, max(c.g, c.b));
        float min_color = min(c.r, min(c.g, c.b));
        float lightness = (max_color + min_color) / 2.0;
        c.rgb = clamp(c.rgb + ((1.0 - lightness) - lightness), 0.0, 1.0);
    }

    o = c * pc.color;
}
