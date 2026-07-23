# NVIDIA scanout: allocate the scanout BO via GBM, not Vulkan

**Status:** IMPLEMENTED (2026-07-20, `diag/scanout-force-tiled` → ready for master). HW-verified
on **oldnas GTX 1050 @ 2560@60** (NVIDIA) and **bee 6900HX APU / RADV RDNA2 @ 2560@60** (AMD):
GBM-allocated tiled block-linear scanout is display-correct with zero missed pageflips over the
full ~32–33 s test on both. (RDNA4-gfx12 still unverified — see "Risks / open questions".)
Vulkan-alloc kept as fallback for the paths listed under "Risks / open questions".
**Related:** `project_nvidia_1060_scanout_device_lost` (padded-LINEAR workaround now
displaced by GBM), the ultrawide drag/idle-cost thread, `project_medion_hybrid_black_screen`
(spec-downgraded — see "codex corrections" below).
**Diag levers:** removed. GBM is the default; no env var required.

## Problem

yserver allocates its **scanout buffer via Vulkan** (`vkCreateImage` with a DRM format modifier +
dma-buf export; see `allocate_vk_scanout_image` in `crates/yserver/src/kms/vk/scanout.rs`), then
`PRIME_FD_TO_HANDLE` + `AddFB2`. On NVIDIA:
- **Tiled (block-linear) scanout garbles** — HW-confirmed on oldnas (GTX 1050 @ 2560) by sweeping
  every advertised gob-height modifier `0x3000000004fe010`…`015` via `YSERVER_SCANOUT_TILED`:
  **all six garble.** AddFB2 + atomic commit *succeed*; the display just reads garbage.
- So yserver forces **LINEAR** (`scanout_prefers_linear` → NVIDIA/Intel), padded to a 256-aligned
  stride for non-aligned widths (the `PaddedExplicitLinear` fix, `4dfb8936`, for 3440 ultrawide).
- LINEAR full-screen compose+scanout every frame is the **ultrawide drag/idle CPU cost** (LINEAR is
  NVIDIA's slow path; scales with pixels — 3440 janky, 2560 borderline, per HW comparison
  oldnas-vs-Peter's-1060).

## Root cause (reference-confirmed against Xorg)

**yserver allocates scanout the non-standard way (Vulkan), instead of via GBM like the whole
ecosystem.** Xorg's modesetting DDX (`../xserver/hw/xfree86/drivers/modesetting/drmmode_display.c`,
`drmmode_create_bo`):
```c
bo->gbm = gbm_bo_create_with_modifiers(gbm, w, h, format, modifiers, count);          // :1150
// fallback:
bo->gbm = gbm_bo_create(gbm, w, h, format, GBM_BO_USE_RENDERING | GBM_BO_USE_SCANOUT); // :1161
drmModeAddFB2WithModifiers(...)                                                        // :1102
```
GBM asks the **driver** for a buffer laid out to be **both renderable and scannable**. On NVIDIA
that includes the block-linear "kind"/scanout tags the *display* engine requires. A Vulkan-allocated
block-linear image is laid out for render/texture use and, exported to KMS, lacks the scanout-correct
layout → the display engine reads garbage. That is precisely why **all gob-height variants garbled**
(wrong allocation *path*, not wrong modifier), and why Xorg/GNOME/mutter display fine on NVIDIA while
yserver must fall back to LINEAR.

**⚠️ Correction to prior belief:** "NVIDIA tiled scanout is a fundamental dead end / garbled" was too
hasty — it's a dead end only via the *Vulkan-alloc* path. GBM-allocated tiled scanout is the standard,
working path.

## Fix

Allocate the scanout BO via **GBM** and **import it into Vulkan** as the compose render target,
instead of Vulkan-allocating + exporting:
1. Hold a `gbm_device` on the KMS DRM fd (yserver already owns the DRM device).
2. `gbm_bo_create_with_modifiers(gbm, w, h, XRGB8888/ARGB8888, kms_plane_modifiers, n)` with the
   plane's advertised modifiers (fall back to `gbm_bo_create(..., RENDERING|SCANOUT)`).
3. Get the bo's fd + modifier + per-plane stride/offset (`gbm_bo_get_fd`, `_get_modifier`,
   `_get_stride_for_plane`, `_get_offset`, `_get_plane_count`).
4. Import that dma-buf into Vulkan as a `VkImage` (`VK_EXT_image_drm_format_modifier`,
   `VkImageDrmFormatModifierExplicitCreateInfoEXT` with the GBM-provided modifier + plane layouts),
   COLOR_ATTACHMENT usage, external memory.
5. Compose renders directly into that imported image (as today), then `AddFB2WithModifiers` on the
   GBM bo.

## Wins
- Correct **tiled** scanout on NVIDIA → fast render+scanout → removes the ultrawide drag/idle cost.
- Likely fixes **medion** black screen (a proper `SCANOUT`-usage buffer on the display GPU, allocated
  by that GPU's allocator, is scanoutable — the current Vulkan-alloc-on-NVIDIA-render vs Intel-display
  mismatch ENOMEMs). (Still also want render-GPU==display-GPU selection — see that memory.)
- Lets us **drop `PaddedExplicitLinear`** (GBM picks a valid stride/layout itself).
- It's simply the correct, ecosystem-standard way to allocate scanout — helps all drivers, not just
  NVIDIA.

## Risks / open questions
- Can Vulkan render into the GBM-imported block-linear modifier image? (Needs COLOR_ATTACHMENT
  support for that modifier on NVIDIA — check `VkPhysicalDeviceImageDrmFormatModifierInfoEXT` /
  `vkGetPhysicalDeviceImageFormatProperties2`.) Very likely yes (it's the driver's own render+scanout
  layout), but verify.
- Multi-plane modifiers (compression): honor `gbm_bo_get_plane_count` / per-plane stride+offset in
  both the Vulkan import and AddFB2 (yserver's scanout path is single-plane today).
- Keep the LINEAR path as a fallback for drivers/planes with no usable tiled scanout modifier.
- Meaty change to the scanout allocation path — verify on RADV (silence, no regression), NVIDIA
  (oldnas/Peter — tiled clean + faster), and the medion Optimus box.

## STEP 0 — codex: rule out the dropped-plane-offset bug FIRST (cheap, may make GBM unnecessary)

codex flagged a concrete simpler bug to eliminate before the GBM rework:
`allocate_vk_scanout_image` queries the full `VkSubresourceLayout` but **keeps only `row_pitch`**
(`scanout.rs:1213`), and `VkScanoutFb` always passes **`offsets = [0,0,0,0]`** to AddFB2
(`scanout.rs:1272`). Even a single-memory-plane block-linear modifier can have a **non-zero plane
offset** — if the tiled image has one and yserver passes 0, the display reads from the wrong place →
garbled, exactly what we saw. **Experiment (on oldnas, uses the existing lever):** log
`layout.offset` for each swept modifier `0x…fe010..015`, pass it into AddFB2, re-sweep.
- If a non-zero offset was being dropped and passing it fixes the garbling → it was a trivial AddFB
  metadata bug, **no GBM rework needed.** Best case.
- If offset is 0 and it still garbles → the KMS ABI has nothing else to pass (handle/pitch/offset/
  modifier only), so the allocation-path theory becomes the leading explanation and GBM is the fix.
  (Note: "GBM adds the scanout kind/tags" stays a proprietary-driver *inference*, not proven by the
  modifier API.)
This is the minimal discriminating experiment: it separates an AddFB-metadata error from a
producer-allocation error decisively. Do it before building GBM.

**RESULT (2026-07-20, oldnas GTX 1050 @ 2560, `YSERVER_SCANOUT_TILED=1` → modifier `0x…fe015`):
`offset=0`, and the display is STILL corrupt.** So STEP-0 is RULED OUT — there was no dropped
offset, the AddFB2 metadata is fully correct (modifier + pitch 10240 + offset 0), yet the
block-linear buffer displays garbage. The KMS ABI carries nothing else (handle/pitch/offset/
modifier), so the corruption is in the buffer's actual CONTENT LAYOUT (Vulkan-written tiling ≠ what
the display engine reads for that modifier). **→ the allocation-path theory is confirmed by
elimination; GBM allocation is the fix. Proceed to the GBM plan below.** (The offset-passing change
is still a correct latent-bug fix to keep — it just wasn't the cause here.)

## codex corrections to the GBM plan (if STEP 0 doesn't resolve it)

- **Vulkan import gate:** query support with the exact tuple (`B8G8R8A8_UNORM`, extent,
  `DRM_FORMAT_MODIFIER_EXT`, exact GBM modifier, `COLOR_ATTACHMENT|TRANSFER_DST|SAMPLED`,
  `VkPhysicalDeviceImageDrmFormatModifierInfoEXT` + `VkPhysicalDeviceExternalImageFormatInfo{DMA_BUF}`)
  and require **IMPORTABLE**, not the current EXPORTABLE check (`scanout.rs:921`). Honor
  `DEDICATED_ONLY`.
- **Import ownership:** keep the `gbm_bo` alive until after FB/GEM teardown AND Vulkan image/memory
  destruction; **dup the fd** before handing to `VkImportMemoryFdInfoKHR` (Vulkan consumes it only on
  success — DRI3 importer does this, `target.rs:355`); use **`vkGetMemoryFdPropertiesKHR`** and
  intersect its `memoryTypeBits` with the image reqs when picking the import memory type (the DRI3
  importer at `target.rs:371` skips this — insufficient for robust external import).
- **Medion/Optimus caveat:** a BO allocated on the **Intel** KMS device is NOT automatically
  importable/renderable by **NVIDIA** Vulkan — external memory must come from a compatible physical
  device. So GBM only fixes medion if the `gbm_device` matches the *render* GPU; this ties back to
  render-GPU==display-GPU selection ([[project_medion_hybrid_black_screen]]). Downgrade the
  "GBM likely fixes medion" claim accordingly.
- **Modifier list** must be the intersection of KMS-plane support and Vulkan **importable
  color-attachment** support (not the current exportable intersection).
- **GBM may pick LINEAR** — don't assume tiled. Treat "GBM tiled render+scanout clean" as a HW
  acceptance test, not a given.
- **Keep the Vulkan-first path as a fallback too** (not only LINEAR): the file notes Venus depends
  on that direction; GBM import may regress it.
- **Multi-plane:** first impl accept only exact **one-memory-plane** GBM/Vulkan modifiers, fall back
  to LINEAR otherwise (preserves `scanout.rs:960`); full multi-plane = per-plane stride+offset to
  both `VkImageDrmFormatModifierExplicitCreateInfoEXT` and AddFB2WithModifiers.
- **Verify beyond render:** solid-color clear → normal composition → page-flips/fences. Successful
  `vkCreateImage`/render does NOT establish scanout correctness.

## Localization (scope is contained) — codex: contained to the subsystem, NOT one function

Allocation chokepoint: `ScanoutBoPool::allocate` → `allocate_with_plan` → `allocate_vk_scanout_image`
(`crates/yserver/src/kms/vk/scanout.rs`), called only from `platform.rs` (init `:787`, resolution
realloc `:2446`). The **compose IS allocation-agnostic** — it renders into `bo.vk_image` /
`bo.vk_image_view`, unchanged. But codex notes the change is **contained to the scanout subsystem,
NOT one/two functions**: `ScanoutBo`'s fields + Drop path assume a Vulkan-owned allocation
(`scanout.rs:202`, `:508`) and need a retained `gbm_bo`/device owner; `VkScanoutFb` is single-plane,
zero-offset only (`scanout.rs:1254`); the `gbm_device` ownership lives in pool/backend state, not the
one helper. Keep LINEAR (and the Vulkan-first path) as fallback.

## Evidence
- oldnas sweep: `YSERVER_SCANOUT_TILED=0x3000000004fe010..015` → all garble; LINEAR (unset) clean.
- `scanout bo: modifier=0x3000000004fe015 succeeded (2560x1440, pitch 10240)` — AddFB2/commit OK, only
  the displayed image is corrupt.
- Xorg modesetting `drmmode_create_bo` uses GBM `RENDERING|SCANOUT` + `AddFB2WithModifiers`.

## Implementation findings (2026-07-20, oldnas GTX 1050 @ 2560@60)

The GBM plan was added as `ScanoutAllocationPlan::GbmModifier(m)`, tried BEFORE every existing
Vulkan-alloc plan for each modifier in the KMS/Vulkan intersection. Per-BO the log now shows
`scanout bo: gbm-modifier=0x…fe015 succeeded (2560x1440, pitch 10240)`, and the display is clean.

- **No lever needed on NVIDIA.** With prefer_linear=true the candidate list is still
  `[LINEAR, tiled...]`, so `GbmModifier(LINEAR)` is tried first — and NVIDIA's GBM REFUSES a
  SCANOUT+RENDERING allocation with modifier=LINEAR (returns NULL). The next plan
  `GbmModifier(0x…fe015)` succeeds with the driver-native block-linear layout. So on NVIDIA the
  plan cascade converges on tiled without any diag flag, matching what Xorg/mutter get.
- **Steady-state (33 s telemetry):** `page_flip/s=59.9–60.0`, `missed_pageflips/s=0` across every
  rollup, `avg_compose_cb_record_ns=0.50 ms` avg / 6.5 ms max, iter_wall_max ~16.6 ms. Rock-solid
  60 Hz.
- **No perceptible perf delta at 2560@60 on this box.** Expected: at that resolution the
  LINEAR-vs-tiled scanout bandwidth difference is a fraction of 1 % of the 1050's VRAM
  bandwidth. The claimed drag/idle-cost win lives at 3440 ultrawide (Peter's 1060) — needs a
  separate HW verify there.

### Quantitative A/B (oldnas GTX 1050 @ 2560@60, xfce -telemetry, ~30 s each)

> **Post-merge (5fdb56eb):** `gpu_render_ns` telemetry was KEPT (now permanent); the
> `YSERVER_SCANOUT_NO_GBM` lever was REMOVED — the baseline commands below are historical
> and no longer runnable as written.

Enabled by two diag additions:
- Silence's `diag(telemetry): wire gpu_render_ns via per-BO timestamp query pool` — 2-query
  TIMESTAMP pool per BO, TOP at CB start, BOTTOM before CB end, read the PREVIOUS compose's
  delta on re-acquire. (Kept — permanent telemetry.)
- `YSERVER_SCANOUT_NO_GBM=1` — skipped the GBM plans so NVIDIA fell back to Vulkan-alloc
  LINEAR (`modifier=0x0`), giving a same-box baseline to compare against GBM-tiled. (Removed
  at merge.)

| metric | **GBM tiled** `0x…fe015` | **Vulkan-alloc LINEAR** `0x0` | delta |
|---|---|---|---|
| `avg_gpu_render_ns` (avg) | **1457 µs** | 1586 µs | **−129 µs (−8.1 %)** |
| `avg_gpu_render_ns` (min) | 1016 µs | 1108 µs | −92 µs |
| `avg_gpu_render_ns` (max) | 2677 µs | 2739 µs | −62 µs |
| `flip/s` | 57.5 | 57.6 | ±noise |
| `missed_pageflips/s` | 0 | 0 | tied |
| `avg_compose_cb_record_ns` (CPU) | 0.58 ms | 0.51 ms | +70 µs (ts writes) |

**Reading:** GBM tiled is ~8 % faster on GPU compose at 2560@60 on the 1050. Reproducible signal
(27 rollups each, tight spread). Not perceptible at this resolution — 120 µs off a 1.5 ms
baseline is rounding-error against a 16.6 ms frame budget — but a real bandwidth-locality win.
Extrapolating linearly to 3440 (~1.8× pixels): LINEAR ~2.85 ms, tiled ~2.62 ms — still modest
in absolute terms. **The dominant reason to prefer tiled at 3440 is correctness** (the
`PaddedExplicitLinear` workaround exists precisely because plain LINEAR at 3440 hit
device-lost on the 1060). Perf is bonus.

### Peter's 1060 @ 3440 A/B — LANDED (2026-07-22)

Result: GBM tiled `avg_gpu_render_ns` ~1.34 ms vs LINEAR ~1.94 ms → **−31 %**, both runs clean
(0 device-lost / atomic-fail / missed-flips), Peter confirms not garbled. The win is far bigger
than the ~8 % extrapolation above — the tiled advantage **scales with resolution** (marginal at
2560, substantial at 3440 ultrawide). GBM shipped to master `5fdb56eb`.

Original recipe (historical — `YSERVER_SCANOUT_NO_GBM` since removed):
- Baseline (LINEAR): `YSERVER_SCANOUT_NO_GBM=1 just yserver-xfce-hw-telemetry`
- Tiled (GBM):       `just yserver-xfce-hw-telemetry`

Then read `avg_gpu_render_ns` steady-state from each `yserver-hw-xfce.log` (grep pattern
`avg_gpu_render_ns=[0-9]+`, drop the first zero-valued startup rollup). Expected pattern
based on oldnas: tiled a few % lower than LINEAR, both 60 Hz clean, LINEAR possibly
device-lost at cold-start on this specific card if it lands on the tight 13760 pitch (pre-
padded-LINEAR fix); with `PaddedExplicitLinear` still in place LINEAR should hold.
- **AMD (RDNA2) — VERIFIED clean on bee (6900HX APU, `driver_id=MESA_RADV,
  device_type=INTEGRATED_GPU`), 2026-07-20 xfce `-telemetry` run.** Every RDNA card was
  previously going through Vulkan-alloc tiled; it now goes through GBM tiled. The GBM cascade
  converged as designed: `candidates=[0x200000010401b03, LINEAR, 0]` → all 3 scanout BOs
  `gbm-modifier=0x200000010401b03 succeeded (2560x1440, pitch 10240)`, **zero fallback to
  Vulkan-alloc or LINEAR, zero GBM failures.** 32 s run, 30 one-second rollups, **every rollup
  `missed_pageflips/s=0`**, steady 60 Hz (`frame_present_count/s`=60 in 23 rollups, 59/61
  jitter otherwise; init rollups aside), no server ERROR/panic/DEVICE_LOST. So the new codepath
  is display-correct + rock-solid on RADV — matches the oldnas GTX 1050 result, no regression.
- **Still-open verification gap: RDNA4-gfx12.** The bee run is RDNA2, NOT RDNA4. GBM tiled on
  RDNA4 is unverified against the #48 LINEAR-corruption fix. If GBM regresses #48, fall back to
  gating GBM plans on NVIDIA/Intel only.

## Codex corrections applied

- **IMPORTABLE gate** (`scanout_modifier_is_single_plane_importable`) is a new peer of the
  EXPORTABLE gate — mirror function factored via `scanout_modifier_single_plane_supports_feature`.
- **Fd ownership** on the Vulkan import: `bo.fd()` → `try_clone()` for Vulkan, retain original
  for `PRIME_FD_TO_HANDLE`. Duped raw fd handed to `VkImportMemoryFdInfoKHR`; Vulkan takes
  ownership on `vkAllocateMemory` success, we `libc::close` it on any failure path.
- **`vkGetMemoryFdPropertiesKHR`** intersected with `mem_reqs.memory_type_bits` before picking
  memory type (fixes the DRI3-importer `target.rs:371` skip codex called out).
- **`gbm_bo` lifetime**: declared as the LAST field on `ScanoutBo` — Rust drops it AFTER the
  explicit Drop impl has torn down FB / GEM / VkImage / VkMemory (matches codex's ordering
  requirement without needing manual `ManuallyDrop` gymnastics).
- **Single-plane only** for the first cut: `bo.plane_count() != 1` returns
  `GbmScanoutError::MultiPlane(n)` and falls through to the next plan. Multi-plane (e.g. AMD
  DCC) is out of scope; the Vulkan-alloc plans already reject multi-plane too, so we're not
  regressing anything.
- **Vulkan-first kept as fallback**: GbmModifier plans first, then existing DrmModifier /
  PaddedExplicitLinear / ExplicitLinear / LegacyLinear. Venus (virtio-gpu) path is preserved:
  if `gbm_create_device` fails on the KMS fd we soft-fall to Vulkan-first, unchanged.

## Deferred (not in this landing)

- **Medion Optimus black screen** is NOT fixed here (Intel KMS + NVIDIA render — GBM on the
  Intel fd produces Intel BOs that NVIDIA Vulkan can't import). Downgraded per codex; needs
  render-GPU==display-GPU selection first.
- **`PaddedExplicitLinear` removal**: kept for now as a Vulkan-alloc fallback if GBM
  totally fails at ultrawide widths. Once we HW-verify GBM on Peter's 1060 @ 3440 we can
  drop the plan and simplify.
- **`avg_gpu_render_ns=0` in telemetry**: the GPU-side timer isn't wired up, so we can't
  measure the actual bandwidth win from tiled scanout from these counters. Separate task
  (query pool + timestamps) to make the ultrawide comparison quantitative.
