#version 450
#extension GL_EXT_scalar_block_layout : require

// RENDER `Composite` fragment shader, convolution variant (phase 2
// of the convolution-filter work, plan
// docs/superpowers/plans/2026-05-15-render-convolution-filter.md).
//
// Used when the source picture has `SetPictureFilter convolution`
// (xfwm4 / marco / mutter / picom all do this for window-shadow
// Gaussian blur). The caller (`try_vk_render_composite`) gates this
// pipeline on identity transform + Repeat::None + no alpha_map; the
// non-convolution path stays in the standard `render.frag.glsl`.
//
// Spec (X RENDER): the convolved value at output pixel (x, y) is the
// sum of products of the kernel weight matrix with the source pixels
// surrounding (x, y). The weights are NOT renormalised here — the
// X RENDER spec says the result is the raw sum, and real client
// kernels (Gaussian shadows) are already normalised by construction.
//
// Edge handling: sampler is CLAMP_TO_EDGE. Caller's Repeat::None
// precondition means out-of-bounds samples are arguably "transparent"
// by spec, but RENDER convolution under Repeat::None typically
// clamps in practice (xfwm4 shadow pixmaps have a fully-transparent
// edge ring, so clamp vs. transparent produce identical output).
// Phase 3 may revisit if a real client cares.

layout(push_constant) uniform PushConsts {
    vec2 dst_origin;
    vec2 dst_size;
    vec2 viewport;
    vec2 src_origin;
    vec2 mask_origin;
    vec2 src_extent;
    vec2 mask_extent;
    ivec2 repeat_modes;
    vec4 src_xform_row0;
    vec4 src_xform_row1;
    vec4 mask_xform_row0;
    vec4 mask_xform_row1;
} pc;

// Cap matches `MAX_KERNEL_DIM` in convolution_pipeline.rs (21×21 =
// 441 weights, 1764 bytes of weight data + 8 bytes header). Real
// xfwm4 / picom shadow blurs are 5×5..11×11.
const int MAX_KERNEL_DIM = 21;
const int MAX_KERNEL_ELEMS = MAX_KERNEL_DIM * MAX_KERNEL_DIM;

// Scalar block layout (`VK_EXT_scalar_block_layout` enabled at
// device-create) tightly packs `float weights[N]` as a real f32
// array — std140 would expand each element to a vec4 slot. The
// matching Rust struct uses `#[repr(C)]` with no padding.
layout(scalar, set = 0, binding = 1) uniform Kernel {
    uvec2 dims;                       // (width, height) — odd integers, both >= 1
    float weights[MAX_KERNEL_ELEMS];  // row-major; only `dims.x * dims.y` entries used
} kernel;

layout(set = 0, binding = 0) uniform sampler2D src_tex;

layout(location = 0) in vec2 v_dst_offset;
layout(location = 0) out vec4 out_color;

void main() {
    // Identity transform / Repeat::None precondition (enforced by
    // caller). The source pixel directly under the output is
    // `src_origin + dst_offset`; +0.5 lands us on the texel centre.
    vec2 src_pixel_center = pc.src_origin + v_dst_offset + vec2(0.5);

    int half_w = int(kernel.dims.x) / 2;
    int half_h = int(kernel.dims.y) / 2;

    vec4 acc = vec4(0.0);
    for (int dy = -half_h; dy <= half_h; ++dy) {
        for (int dx = -half_w; dx <= half_w; ++dx) {
            vec2 sample_pixel = src_pixel_center + vec2(float(dx), float(dy));
            vec2 uv = sample_pixel / pc.src_extent;
            int idx = (dy + half_h) * int(kernel.dims.x) + (dx + half_w);
            acc += texture(src_tex, uv) * kernel.weights[idx];
        }
    }
    out_color = acc;
}
