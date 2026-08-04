// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

#version 450

// One direction of a separable gaussian, ported from mutter's `clutter-blur.c:72-109`.
//
// The kernel is evaluated *incrementally* (K. Turkowski, GPU Gems 3 ch. 40) rather than from a
// precomputed table: three running coefficients are multiplied forward each step, so the shader
// takes sigma as a uniform and needs no per-radius specialization.
//
// Each step covers two taps with one fetch by landing between them — `gauss_ratio` is where
// between, so the hardware's linear filter does the weighting. That is why the loop advances by 2
// and why `n_steps` is `ceil(1.5 * sigma) * 2`: 1.5 sigma each side is where the tail stops
// mattering.
//
// `direction` picks the pass (1,0 horizontal, 0,1 vertical) and `pixel_step` is one texel in the
// sampled texture's UV space. `brightness` is the multiply ShellBlurEffect does in its own pass
// (`shell-blur-effect.c:47-51`); folding it into the second direction saves a full-target pass and
// is the same arithmetic.

layout(set = 0, binding = 0) uniform sampler2D tex;

layout(push_constant) uniform Push {
    vec2 direction;
    float pixel_step;
    float sigma;
    float brightness;
} pc;

layout(location = 0) in vec2 v_uv;
layout(location = 0) out vec4 o;

void main() {
    vec3 gauss_coefficient;
    gauss_coefficient.x = 1.0 / (sqrt(2.0 * 3.14159265) * pc.sigma);
    gauss_coefficient.y = exp(-0.5 / (pc.sigma * pc.sigma));
    gauss_coefficient.z = gauss_coefficient.y * gauss_coefficient.y;

    float gauss_coefficient_total = gauss_coefficient.x;

    vec4 ret = texture(tex, v_uv) * gauss_coefficient.x;
    gauss_coefficient.xy *= gauss_coefficient.yz;

    int n_steps = int(ceil(1.5 * pc.sigma)) * 2;

    for (int i = 1; i <= n_steps; i += 2) {
        float coefficient_subtotal = gauss_coefficient.x;
        gauss_coefficient.xy *= gauss_coefficient.yz;
        coefficient_subtotal += gauss_coefficient.x;

        float gauss_ratio = gauss_coefficient.x / coefficient_subtotal;

        float foffset = float(i) + gauss_ratio;
        vec2 offset = pc.direction * foffset * pc.pixel_step;

        ret += texture(tex, v_uv + offset) * coefficient_subtotal;
        ret += texture(tex, v_uv - offset) * coefficient_subtotal;

        gauss_coefficient_total += 2.0 * coefficient_subtotal;
        gauss_coefficient.xy *= gauss_coefficient.yz;
    }

    o = ret / gauss_coefficient_total;
    o.rgb *= pc.brightness;
}
