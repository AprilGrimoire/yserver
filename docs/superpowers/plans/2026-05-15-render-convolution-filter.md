# RENDER convolution filter — Phase 2 (Vk pipeline + integration)

Branch: `render-convolution-filter` (off `graphics-followups`).
Phase 1 (`898dd19`): SetPictureFilter wire parse + `PictureFilter`
field on `PictureState::Drawable`. No rendering effect yet.

This plan covers phase 2: making the parsed filter actually affect
the rendered output for the `Convolution` case. xfwm4's compositor
relies on this for drop-shadow blur — without it, shadows render as
hard-edged flat rectangles (see `docs/known-issues.md`, "KMS:
render_set_picture_filter is currently a no-op").

## Goal

When a Composite operation reads a source picture whose
`PictureState::Drawable::filter` is `PictureFilter::Convolution`,
sample the source through the kernel rather than via the fixed
LINEAR sampler. Expected user-visible result: xfwm4 menu / window
shadows render as smooth Gaussian fades, not flat blocks.

## Non-goals (this phase)

- Convolution with arbitrary transforms (rotate / scale).
  Compositors apply convolution to identity-transformed pictures;
  if a transform IS present, fall back to LINEAR sampling for now
  and log at debug.
- Persistent shader cache for the convolution pipeline.
- Mask-side convolution (only source-side filter is honored).
  Real compositors rarely use convolution on the mask picture.
- Convolution combined with `alpha_map` or non-`Repeat::None` —
  edge handling under repeat semantics needs spec study; fall back
  to LINEAR sampling for those cases initially.

## Approach: single-pass arbitrary kernel

The simplest implementation that matches xfwm4's usage. xfwm4's
typical kernel size for menu shadows is 5×5 or 7×7 (≤49 weights).

- New `kms/vk/convolution_pipeline.rs` parallel to `render_pipeline.rs`.
- New GLSL fragment shader: loops over kernel weights, samples source
  picture at offsets `(-w/2 + i, -h/2 + j)` from output pixel, sums
  weighted samples.
- Kernel weights + dimensions in a uniform buffer (descriptor set
  binding), since 128-byte push constants are already saturated by
  `RenderPushConsts`. Cap kernel size to e.g. 21×21 (441 weights ×
  4 bytes = 1.7 KiB) — comfortably under any UBO limit.

Two-pass separable Gaussian was considered but rejected for phase 2:
- Convolution filter is generic (any kernel, not just Gaussian).
- The "is this kernel separable?" detection is fiddly.
- Single-pass arbitrary-kernel handles xfwm4 fine.

If profiling shows convolution is a hot path post-deploy, revisit
with a two-pass separable variant.

## Task breakdown

### T1 — pipeline scaffold

Create `kms/vk/convolution_pipeline.rs`:
- `ConvolutionPipeline` struct (mirrors `RenderPipelineCache`'s shape
  but simpler — one fixed PictOp `Over`, one dst format `B8G8R8A8`).
- Descriptor set layout: 1 sampler binding (source picture), 1 UBO
  binding (kernel data).
- Push constants: same `RenderPushConsts` shape (128 bytes) — reuse
  the existing struct. Adds nothing new; transform & dst geometry
  via the same path.
- Lazy build on first use; cached.

GLSL: new `convolution.frag.glsl` + reuse `render.vert.glsl`.
Fragment shader:
```glsl
layout(set=0, binding=0) uniform sampler2D source;
layout(set=0, binding=1) uniform Kernel {
    uvec2 dims;            // width, height
    float weights[441];    // row-major; max 21×21
} kernel;

void main() {
    vec4 acc = vec4(0.0);
    float weight_sum = 0.0;
    vec2 base_uv = ...;    // output pixel → source UV (from existing math)
    int half_w = int(kernel.dims.x) / 2;
    int half_h = int(kernel.dims.y) / 2;
    for (int dy = -half_h; dy <= half_h; ++dy) {
        for (int dx = -half_w; dx <= half_w; ++dx) {
            vec2 uv = base_uv + vec2(float(dx), float(dy)) / source_size;
            float w = kernel.weights[(dy+half_h) * int(kernel.dims.x) + (dx+half_w)];
            acc += texture(source, uv) * w;
            weight_sum += w;
        }
    }
    out_color = acc / max(weight_sum, 1e-6);
}
```

Edge handling: by default the source sampler is `CLAMP_TO_EDGE`, so
samples outside the source picture clamp to its edge pixel. Matches
RENDER's default repeat behavior for `Repeat::None`. For other repeat
modes, defer to phase 3.

### T2 — wire build.rs

Update `crates/yserver/build.rs` to compile `convolution.frag.glsl`
to SPIR-V. Mirror the existing render-shader build entry.

### T3 — kernel UBO allocation

Add `ConvolutionKernelUbo` to backend state — a single per-frame UBO
slot that we write into before each convolution composite. Kernel
weights bounded; 1.7 KiB max. Allocate `HOST_VISIBLE | HOST_COHERENT`
memory; map persistently; memcpy weights on each Composite.

If multiple convolution composites in one PaintBatch reuse the same
kernel (likely for shadow runs), we could memoize — but optimize
later. Phase 2 just writes every time.

### T4 — integrate into Composite path

In `try_vk_render_composite` (kms/backend.rs:1894 / vk/ops/render.rs):
- Resolve source `PictureState`. If `Drawable { filter: Convolution { ... }, transform: None, alpha_map: None, repeat: Repeat::None }`, take the convolution branch.
- Otherwise (any other filter or unsupported attribute combo with
  convolution), fall through to the existing render pipeline path.
- Convolution branch:
  - Memcpy kernel weights into the UBO.
  - Bind `ConvolutionPipeline`, descriptor set, push consts.
  - Issue draw (same quad as the render pipeline).
- Pipeline barriers / batch integration: identical to the existing
  `record_paint_batch_op` flow; convolution events go through
  `record_paint_op` like other RENDER ops.

### T5 — tests + smoke

- Unit: convolution-pipeline construction + UBO write smoke test.
- Hardware smoke:
  - `just yserver-xfce-hw` — open Applications menu, verify the
    shadow is now a smooth fade rather than a flat block (compare
    to a host Xorg+xfce baseline screenshot).
  - `just yserver-mate-hw` — marco shadows.
  - Regression: ensure non-convolution clients (caja, thunar)
    composite identically.

### T6 — known-issues update

Mark the `render_set_picture_filter is currently a no-op` entry as
fixed (with a note about the bilinear-only behavior for picturs
without convolution). The 4.1.4.6 follow-up about multiple samplers
per filter mode is still open (different concern from convolution).

## Open questions for phase-2 start

- **Repeat::Pad / Reflect with convolution.** Phase 2 falls back to
  LINEAR if repeat ≠ None. Is this acceptable for xfwm4? Likely yes —
  shadow pixmaps are smaller-than-screen with explicit edges, and
  xfwm4 uses Repeat::None. Verify when integrating.
- **Mask + convolution.** xfwm4's shadow flow composites source
  (the shadow pixmap) with mask (the window-shape alpha mask).
  Convolution on source only — mask sampling stays LINEAR. Need to
  verify the convolution-output then mask path renders correctly;
  may need a small temp texture for the convolution output if mask
  blending requires it as an input to a second composite.
  → Build T4 with this in mind; defer the temp-texture path if first
  implementation works without it.

## Estimated effort

T1 (pipeline scaffold + shader): ~1-2h.
T2 (build.rs): ~10 min.
T3 (UBO allocation): ~30 min.
T4 (integration + branch in render): ~30 min.
T5 (tests + smoke): ~30 min.
T6 (docs): ~10 min.

Total: ~3-4h focused. Single session likely.
