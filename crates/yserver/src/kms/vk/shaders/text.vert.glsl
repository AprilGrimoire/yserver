#version 450

// Batched glyph quad vertex shader (#1 glyph draw batching). One
// `vkCmdDraw(4, N_glyphs, ..)` with TRIANGLE_STRIP topology draws the
// whole run: the 4-vertex quad comes from `gl_VertexIndex`, and each
// glyph's destination rect + atlas rect arrive as per-instance vertex
// attributes (VK_VERTEX_INPUT_RATE_INSTANCE). Per-run constants
// (viewport, atlas extent, foreground) stay in push constants.
//
// Atlas coords are in TEXELS; this shader divides by `atlas_extent`
// so recorded instance data is independent of any later atlas resize.
//
// NDC convention matches `composite.vert.glsl`: y increases
// downward; pixel `(0, 0)` lands at NDC `(-1, -1)` (top-left).
//
// Push-constant layout is plain std430. This block is `vec2, vec2,
// vec4`: the two `vec2`s exactly fill bytes 0..16, so `foreground`
// lands at offset 16 under std430 — identical to what scalar layout
// would give, and matching the host `TextPushConsts` `repr(C)` struct
// (whose compile-time asserts lock size==32 / atlas_extent@8 /
// foreground@16). No `GL_EXT_scalar_block_layout` needed, which lets
// the shader run on devices without the optional `scalarBlockLayout`
// feature (Broadcom V3D / v3dv on the RPi 4/400).

layout(location = 0) in vec2 dst_origin;  // pixels
layout(location = 1) in vec2 dst_size;     // pixels
layout(location = 2) in vec2 atlas_xy;     // texels
layout(location = 3) in vec2 atlas_wh;     // texels

layout(push_constant) uniform PushConsts {
    vec2 viewport;
    vec2 atlas_extent;  // texels
    vec4 foreground;    // RGB used by fragment shader; alpha is 1.0
} pc;

layout(location = 0) out vec2 v_uv;
layout(location = 1) out vec4 v_foreground;

void main() {
    vec2 quad = vec2(float(gl_VertexIndex & 1), float((gl_VertexIndex >> 1) & 1));

    vec2 dst_pixel = dst_origin + quad * dst_size;
    vec2 ndc = dst_pixel / pc.viewport * 2.0 - 1.0;
    gl_Position = vec4(ndc, 0.0, 1.0);

    v_uv = (atlas_xy + quad * atlas_wh) / pc.atlas_extent;
    v_foreground = pc.foreground;
}
