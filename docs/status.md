# Status — Rendering Re-architecture

Working doc for the rendering re-architecture program tracked in
`docs/superpowers/specs/2026-05-12-rendering-rearchitecture.md` and
phase-detail plans under `docs/superpowers/plans/`.

The previous status doc (covering phases 1–6 and the host-X11 era)
is archived at `status-archive-2026-05-13.md`. Re-read it for
historical context; new work tracks here.

Cross-cutting bugs and followups that don't fit a phase live in
[`known-issues.md`](known-issues.md).

---

## Phase progress

### Done

- [x] **Phase 3A — Infrastructure** (`4af9e01`)
  - `PaintBatch` state machine: `Idle → Recording → Closed → Submitted → Retired`, plus `Poisoned` terminal.
  - `BatchUploadArena` (chunked host-visible bump allocator, 1 MiB → 64 MiB).
  - `BatchDescriptorArena` (per-batch descriptor pool, chunk-grown).
  - `BatchFlushReason` enum with strict/best-effort semantics.
  - `KmsBackend::record_paint_batch_op` (wide API) + `record_paint_op` (shim).
  - `paint_resources()` borrow-split helper, gates on `renderer_failed`.
  - Layout-state policy + CPU-visible / sync-export audit.
  - Plan: `docs/superpowers/plans/2026-05-12-rendering-rearchitecture-phase3.md`
  - Results: `docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase3a-results.md`

- [x] **Phase 3B — Fill + copy-distinct + copy-same** (`82558a5`)
  - Migrated `try_vk_solid_fill`, `try_vk_fill_with_function`, fill-mirror-solid; `copy_area_distinct` (4 sites) and `copy_area_same` (1 site).
  - `run_legacy_paint_op` wrapper for non-migrated recorders.
  - `renderer_failed` gate on all paint paths.
  - Drawable-destruction barriers (salvage after AMD VM_CONTEXT fault) at 5 sites: `DestroyWindow`, `configure_window` resize, `FreePixmap`, `RenderFreePicture`, `RenderCreateCursor` rescue path. Strict-flush failure preserves lifetime invariant via `mem::forget` / leave-in-place.
  - `feedback_kmsbackend_drop_order` + `feedback_paintbatch_destruction_barrier` memories.
  - Results: `docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase3b-results.md`

- [x] **Phase 3C — PutImage + cursor mirror** (`ac270d9`)
  - Migrated `try_vk_put_image` and `upload_bgra_to_mirror` to per-batch `BatchUploadArena` staging.
  - Gradient-create `flush_if_needed(ProtocolBarrier)` (conservative protocol boundary).
  - Outer-flag OOM-poison-avoidance pattern (both T1 + T2 fixes folded after codex review).
  - Results: `docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase3c-results.md`

- [x] **Phase 3D — copy-same-overlap** (`47f213f`)
  - Migrated `try_vk_copy_area` same-overlap arm to `record_paint_batch_op`.
  - `CopyScratch::needs_grow` + pre-resize `flush_if_needed(ProtocolBarrier)` to prevent the dangling-image hazard (`ensure_size`'s `queue_wait_idle` doesn't wait for un-submitted commands).
  - Results: `docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase3d-results.md`

- [x] **KMS teardown fix** (`a693255`) — inter-phase, between 3D and 3E
  - Codex-pinpointed P0: pre-fix shutdown left DRM framebuffers bound to CRTCs, breaking host Wayland sessions (`labwc`/`dms`) until reboot.
  - 6-step `disable_output`: stop composites → flush PaintBatch → vkDeviceWaitIdle → drain DRM pageflip-completes → atomic disable → force-reset (success) / disarm (failure).
  - `ScanoutBo::disarm` + `Buffer::disarm` for failed-output paths (RAII Drop becomes no-op; resources leak until DRM-fd close at process exit reaps).
  - Atomic `disable_output` itself still EINVALs (see followups); disarm makes it harmless.
  - Plan: `docs/superpowers/plans/2026-05-13-kms-teardown-fix.md`
  - Results: `docs/superpowers/plans/2026-05-13-kms-teardown-fix-results.md`

- [x] **Phase 3E — Text-run** (`492b4bc`)
  - Migrated `try_vk_text_run` and `try_vk_render_composite_glyphs` to `record_paint_op`.
  - Single `paint_resources()` call before the intern loop (gates atlas upload on `renderer_failed`).
  - `GlyphAtlas::intern` intentionally unchanged (its per-glyph `queue_wait_idle` is phase-5 sync-rework scope).
  - Hardware smoke: MATE renders, gedit fast text-scroll observed.
  - Interleaved fixes during 3E smoke: composite-pool-release per-frame (`cb44c1d`), Composite mode-constant attempt + revert (`92a2a83` → `3751c11`, filed).
  - Results: `docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase3e-results.md`

- [x] **Phase 3F-1 — Render-composite migration** (`fade626` + fix-ups `c4a4965`)
  - Migrated `try_vk_render_composite` to `record_paint_batch_op`. Descriptor set allocated per-batch via `RenderPipelineCache::allocate_descriptor_for_views_into` + `BatchDescriptorArena` (T2, `3fe108b`). `DstReadback::needs_grow` accessor (T1, `afc18f6`) + pre-resize flush gate prevents the same dangling-image hazard 3D fixed for `CopyScratch`.
  - Unconditional pre-record `ProtocolBarrier` flush before each RENDER Composite is gone; composite-heavy frames now pack into the open `PaintBatch` alongside fill / copy / put_image / text.
  - `try_vk_render_traps_or_tris` and the legacy shared-pool allocator + `reset_descriptors` deliberately retained for 3F-2.
  - Hardware smoke: TBD (user-owned).
  - Results: `docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase3f-1-results.md`

- [x] **Phase 3F-2 — Render-traps/triangles migration + MaskScratch arena** (`f242945` + cleanup `df5dbba`)
  - Migrated `try_vk_render_traps_or_tris` (the last paint-side recorder on legacy `run_one_shot_op + ProtocolBarrier flush`) to `record_paint_batch_op`. Mask coverage staging moved from `MaskScratch`'s private buffer to per-batch `BatchUploadArena` via the new `MaskScratch::record_upload_r8` + `needs_image_grow` + pub `ensure_image_size` trio.
  - Removed the legacy `RenderPipelineCache::reset_descriptors` / `allocate_descriptor_for_views` / `descriptor_pool` field (T4). All RENDER paths now allocate descriptors per-batch via `BatchDescriptorArena`.
  - Audit catalogue: traps row label corrected from "try_vk_render_traps (composite)" to "try_vk_render_traps_or_tris".
  - Hardware smoke: TBD (user-owned).
  - Results: `docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase3f-2-results.md`

### Inter-phase chores landed alongside

- [x] **Composite defer log summary** (`4c4741b`) — turn per-frame `pool_ring_exhausted` warn-spam into a periodic 5s `info!` summary.
- [x] **Scroll-wheel support** (`b7d17a1`) — `InputEvent::PointerScroll` + libinput axis translation + synthetic-button-code mapping to X11 buttons 4/5/6/7. `has_axis` fix (`56f93d9`) closes a libinput "client bug" log flood.
- [x] **Composite pool-release per-frame** (`cb44c1d`) — fixed a pre-existing FIFO-drain bug where one lagging output held pool slots hostage for already-retired frames on the other output. Caught by codex during 3E smoke.

- [x] **Phase 4 — Sync rework (close-time wait)** (`2135a16` + `642d544` + `6fe4a71` + `49ff484` + `f68d8c2`)
  - T1: replaced `vkQueueWaitIdle` in `PaintBatch::submit_and_wait` with `wait_for_fences` on a per-batch `VkFence`. Narrower wait scope.
  - T2: added `submit_async` / `try_retire_if_signaled` / `wait_for_completion` async-retirement building blocks.
  - T3: `RenderScheduler` gained `submitted_paint_batches` queue + `close_and_submit_async` + `poll_retired_paint_batches`. `flush_if_needed` branches strict (blocking) vs best-effort (async). Poll wired into composite tick.
  - T4: `MAX_IN_FLIGHT_PAINT_BATCHES = 4` backpressure cap on the queue.
  - T5: `drain_submitted_paint_batches` called after `vkDeviceWaitIdle()` in shutdown.
  - **Hardware smoke: confirmed on fuji (2026-05-14)** — heavy GTK use (GIMP drag, steady-state mate session) is now low-CPU and lag-free; "snappy as fuck" per user. Adapta + mate-cc burst case unchanged (separate workload, separate phase below).
  - Results: `docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase4-results.md`

- [x] **Phase 5 — Targeted `VkFence` for run_one_shot_op + scratch grow defer-release** (`604f009` + `c6dfecc` + `eea0316` + `067b6c3` + `43dd62c` + `11321b6` + this T7 commit)
  - T1: `run_one_shot_op` swapped `queue_wait_idle(graphics_queue)` for per-op `VkFence` + `wait_for_fences`. 5-path failure taxonomy (extends Phase 4's 4-path model with pre-submit failure window of `begin` / `record` / `end`). `cb_safe_to_free` flag gates outer CB free. 5 in-scope callers (`hw_cursor_refresh`, `read_mirror_pixels`, `try_vk_get_image_pixels`, `dump_scanout_one`, `run_legacy_paint_op`) latch `renderer_failed` on Err.
  - T2: `RenderScheduler::defer_resource_release` adopts a `BatchResource` into the open paint batch (lazy-Idle-open) when any live batch might reference it; synchronous release otherwise. `Poisoned` current batch is discarded before deciding (Drop on Poisoned is no-op → can't host adoptions). Companion `defer_resource_release_decision_for` pure helper + 10-case test matrix.
  - T3 / T4 / T5: `CopyScratch::ensure_size_returning_old` / `DstReadback::ensure_returning_old` / `MaskScratch::ensure_image_size_returning_old` return the OLD image wrapped as `Retired*Image: BatchResource`; callers defer-release through the scheduler. The three pre-flush gates (3D `CopyScratch`, 3F-1 `DstReadback`, 3F-2 `MaskScratch+DstReadback`) are entirely gone.
  - T6: redundant `queue_wait_idle`s deleted from `OpsStaging::ensure` and `GlyphAtlas::grow_staging`. Post-T1, every caller of these grow paths goes through `run_one_shot_op` whose per-op fence already retired the OLD buffer's last referencing CB. Audit comments left at both sites.
  - Hardware smoke: TBD (user-owned).
  - Results: `docs/superpowers/plans/2026-05-14-rendering-rearchitecture-phase5-results.md`

- [x] **Pixmap-allocation pool — burst-absorbing `VkImage` recycling** (`850bb9c` + `9443a2e` + `8b3f243` + `2966407` + `a7c2384` + this T6 commit)
  - Burst-absorbing recycling of server-owned pixmap `VkImage` + `VkImageView` + `VkDeviceMemory` triples, keyed by `(width, height, format)`. Closes the kernel-allocator burst hot path under adapta-nokto + mate-cc cross-vendor reproducer (catastrophic pre-pool on bee/RDNA2 + Arch and fuji/Intel + Arch).
  - T1 (`850bb9c`): `PixmapPool` infrastructure — new `crates/yserver/src/kms/vk/pixmap_pool.rs` with `PixmapPool` + key/entry/`PooledPixmapReturn` BatchResource + `try_take` / `try_return` / `stats` / `drain`. `Arc<Mutex<HashMap<…>>>` shape per the codex Round-1 P0 fold (`Arc<RefCell<…>>` doesn't satisfy `BatchResource: Send`). `DrawableImage::new_from_pool` constructor.
  - T2 (`9443a2e`): `free_pixmap` synchronous-flush gone from the common path; mirrors adopt as `PooledPixmapReturn` BatchResources into the currently-open paint batch via Phase 5 T2's `defer_resource_release`. Eligibility + bucket-cap decided at retire time (codex Round-3 P0 — uniform defer-release for all Vulkan-up server-owned mirrors). DRI3-imported variant routes to flush+drop fallback (T2 reviewer agent caught `into_pool_entry` panics for `ImageBacking::Imported`).
  - T3 (`8b3f243`): `allocate_pixmap_mirror` consults `self.pixmap_pool.as_ref().and_then(|p| p.try_take(key))` before falling through to `new_server_owned_pixmap`. Pool hit skips `vkCreateImage` + `vkAllocateMemory` entirely.
  - T4 (`2966407`): shutdown drain — `pixmap_pool.drain()` called after `scheduler.drain_submitted_paint_batches()`; defensive `Arc::strong_count > 1` warning catches Phase-4-T5 ordering bugs.
  - T5 (`a7c2384`): synthetic burst test (100 pixmaps × 2 bursts → `total_hits == 32 == PIXMAP_POOL_BUCKET_CAP`); `pixmap_pool_stats` + `force_retire_in_flight_for_test` accessors as Pattern A pub fns (cfg(test)-on-impl doesn't reach integration test crates per codex Round-1 P1).
  - Hardware smoke: TBD (user-owned). The load-bearing test: bee + fuji under adapta-nokto + mate-cc. If `bee` improves, AMD-specific investigation is no longer next-priority.
  - Results: `docs/superpowers/plans/2026-05-14-pixmap-allocation-pool-results.md`

- [x] **Diagnostics + perf-followup wins** (`6e3fda7` + `147b1c5` + `95421e5` + `ca8ec6d`, all in `pre-timeline-rework` tag at `8daa1b1`)
  - Per-Vulkan-call rate counters (`kms/vk/call_stats.rs`) with `YSERVER_LOOP_TELEMETRY=1` env-gated per-second emission: `barrier2`, `draw`, `bind_pl`, `bind_ds`, `push_const`, `viewport`, `scissor`, `begin_rendering`, `end_rendering`, `copy_*`, `clear_color_image`, `queue_submit2`, `begin_cb`, `end_cb`.
  - Per-source `queue_submit2` attribution (`vk submit src` line): `vis_composite`, `readback`, `ext_sync`, `protocol_barrier`, `size_limit`, `latency_limit`, `shutdown`, `one_shot`, `compositor`, `other`. Sum tracks queue_submit2 within Idle-flush noise.
  - ProtocolBarrier site-subdivision (`vk pb src` line): `drawable_destroy`, `window_resize`, `image_dealloc_fb`, `dmabuf_release`, `picture_destroy`, `cursor_picture` (gradient sites removed alongside their flushes).
  - `submit_other` (init_clear) site-subdivision: cursor / window / pixmap.
  - PixmapPool per-second deltas (`pixmap pool` line): `takes_hit`, `takes_miss`, `returns_accepted`, `returns_rejected_bucket_full`, `returns_rejected_oversize`. Exposed via `Mutex<Weak<PixmapPool>>` registered at construction.
  - **Win — gradient pre-build flush removed** (`ca8ec6d`): the `flush_if_needed(ProtocolBarrier)` at the top of `render_create_linear_gradient` + `render_create_radial_gradient` was documented as "hygiene cleanup … not a UAF fix". Telemetry showed 50-90 gradient creates/sec on both bee and fuji during MATE drag, each forcing a strict queue submit + wait. Removed; ~25% drop in fuji's `queue_submit2` peak (1430→1047/sec).
  - **Telemetry findings** (recorded for future work):
    - Picture destruction (`picture_destroy`) was the dominant ProtocolBarrier source — peaks ~975/sec on bee, ~888/sec on fuji during MATE drag.
    - On fuji, `window_resize` settled to a sustained 80/sec steady-state pattern paired with 80/sec `init_clear_window` — something in MATE resizes a window 80×/sec independent of user input. Worth understanding eventually.
    - Pool `rejected_oversize` peaks ~253/sec on bee (matched `init_clear_pixmap` peak): MAX_POOLED_DIM=128 makes large pixmaps bypass the pool entirely. Raising the cap is a candidate follow-up but has memory-tradeoff implications.

- [x] **GPU trap rasterization — RENDER Trapezoids / Triangles on the GPU** (`151c8be` (plan) + `0f5e605` + `4dd56a6` + `b7d0e77` + `c819a52` + `4fead28` + `5bf046b` + this T6 commit)
  - Moved RENDER `Trapezoids` / `Triangles` coverage-mask generation off the CPU (where it was 19.73% of CPU per the bee/RDNA2 + adapta-nokto perf trace) onto the GPU via a new `TrapPipeline`. The synchronous CPU rasterize + `MaskScratch::record_upload_r8` upload pair on the X protocol request handler's hot path is gone — `try_vk_render_traps_or_tris` is pure-recording (deferred through `record_paint_batch_op`), so the input loop returns in microseconds instead of blocking per request. The 19.73% CPU cost is zero by construction (code path no longer exists).
  - T1 (`0f5e605`): `TrapPipeline` infrastructure — new `crates/yserver/src/kms/vk/trap_pipeline.rs` with two pipelines (trap + triangle) sharing one push-const layout (no descriptor sets — per-instance data via vertex attributes). `TrapInstanceData` (40 bytes), `TriangleInstanceData` (24 bytes), `TrapDrawPushConsts` (32 bytes: mask_extent + bbox). `Trapezoid::to_instance_data` + `Triangle::to_instance_data` conversion helpers. `BatchUploadArena` buffer usage gained `VERTEX_BUFFER` flag. Two new shaders (trap.vert/frag.glsl) compiled to SPIR-V via existing build.rs. No caller wired.
  - T2 (`4dd56a6` + fix-up `b7d0e77`): trapezoid arm wired to GPU. `KmsBackend.trap_pipeline` field + dual ctor init. Per-batch arena upload + `vkCmdDraw(4, n_traps)` into MaskScratch. Mask-LOCAL vertex emit + bbox-origin pushed to fragment so MaskScratch coords always start at (0, 0) where `record_render_composite` reads. Fix-up deferred `set_current_layout` until after fallible record steps succeed (codex T2 P2 — prevents CPU-tracked layout drifting from GPU state on failure).
  - T3 (`c819a52` + fix-up `4fead28`): triangle arm wired to GPU. Triangle shader handles RENDER's no-fixed-winding-convention via vertex-shader signed-area-orientation + flat-interpolated `orient` sign. Fix-up flipped winding sign (codex T3 P1 — `-1.0` for CCW per the `(-d.y, d.x)` perpendicular convention) and added explicit `orient = 0.0` degenerate handling with fragment `discard` (codex T3 P2 — collinear-but-nonzero-length-edge triangles weren't rejected).
  - T4 (partial): rendercheck triangles 456/456 PASS, blend 4/4 PASS, coord PASS; composite/cacomposite/gradients didn't complete in T4's time budget. No regressions on what ran. Full validation deferred to user-owned hardware smoke (the linear coverage approximation does NOT need the wedge fallback based on the data so far).
  - T5 (`5bf046b`): dead-code deletion — `rasterize_trapezoids` + `rasterize_triangles` + `MaskScratch::record_upload_r8` deleted. `rg` returns one doc-comment historical note; zero call sites. The 19.73% CPU cost in the bee adapta-nokto perf trace is gone by construction.
  - Hardware smoke: TBD (user-owned). Load-bearing test: bee adapta-nokto + mate-cc post-pool + post-GPU-trap should be dramatically improved; window-drag CPU should drop materially; rendercheck full run should be no-regression vs Phase 5 baseline.
  - Results: `docs/superpowers/plans/2026-05-14-gpu-trap-rasterization-results.md`

### Paused — 2026-05-14

- [⏸] **Timeline-semaphore migration — picture defer-release attempt** (commits `d8cfe42`, `0cd4b9e`, `00f3817`, `ea3c6f2`, `445d732`, `b03b585`, `cb73af4`, `73b3728` on `graphics-followups`; rollback tag `pre-timeline-rework` at `8daa1b1`).
  - **Motivation.** The picture_destroy strict flush (~900/sec on both machines during MATE drag) was the dominant ProtocolBarrier source. Attempted to convert it to a defer-release through a new timeline-keyed model.
  - **Plan**: `docs/superpowers/plans/2026-05-14-timeline-semaphore-migration.md`. Six stages; only 1-4 + ad-hoc fixes attempted.
  - **What works** — **fuji** with the full stack (HEAD = `73b3728`):
    - `queue_submit2` peak dropped from 1430/sec baseline → ~700/sec.
    - `picture_destroy` rate: 0 (defer-release path active).
    - User report: "can't make it lag" — perceived as smooth.
  - **What doesn't work** — **bee**: still GPU-faults under MATE drag. Same fault fingerprint across every iteration (amdgpu `PERMISSION_FAULTS:3`, TCP or CB/DB client, RW=0 or 1). Multiple fixes attempted (handle=0 flush fallback, pool path migration, `Arc::strong_count` check on defer-queue gc, `queue_wait_idle` on `destroy_window`); none stabilised bee.
  - **Why bee specifically** — RADV's TCP / CB/DB hardware catches freed-VA reads/writes; ANV swallows them silently. So fuji's "working" state may also have latent UAFs that just don't surface.
  - **Architectural gap that wasn't closed** — the compositor submits its CB on the same `graphics_queue` as paint batches but isn't in `submitted_paint_batches`, doesn't annotate handles via the touched/timeline system, and isn't drained by `flush_if_needed(ProtocolBarrier)`. Any resource sampled by the compositor (window mirrors, cursor mirror, bg_pixmap, etc.) has a lifetime hole when freed via paths that only flush paint. Stage 2 should have included compositor-side handle annotation as a prerequisite; trying to retrofit it through call-site patches didn't converge.
  - **Process lesson** (`feedback_stop_iterating_when_fixes_dont_land` memory): same fault signature across 5 iterations was the cue to revert, not stack more patches. The last few "small fixes" were hunch-based.
  - **State left for resumption**:
    - `pre-timeline-rework` tag is the known-stable baseline. Reset there if bee stability is needed immediately (loses fuji's win).
    - The work past the tag is preserved on `graphics-followups`; it should NOT ship as-is.
    - Next attempt needs the comprehensive design pass with codex — every submission path (paint, compositor, one-shot, init_clear) must signal the timeline AND annotate handles, BEFORE migrating any consumer call site.

### In progress

- [~] **RENDER convolution filter** — on
  `render-convolution-filter` branch off `graphics-followups`.
  Phase 2 code complete but **no real-world consumer reaches the
  path on yserver** (see "Hardware smoke outcome" below); branch
  stays uncommitted pending a consumer.
  - Phase 1 done (`898dd19`): `SetPictureFilter` wire body parsed;
    `PictureFilter::{Nearest, Bilinear, Convolution{w, h, weights}}`
    stored on `PictureState::Drawable`. No rendering effect yet.
  - Phase 2 code complete (uncommitted, validated against lavapipe):
    `docs/superpowers/plans/2026-05-15-render-convolution-filter.md`.
    - T1: new `kms/vk/convolution_pipeline.rs` —
      `ConvolutionKernelData` (8 B header + 441 × f32 = 1772 B,
      static-asserted, with `from_parsed` + size/dim validation) +
      `ConvolutionPipeline` (sampler NEAREST + CLAMP_TO_EDGE, 2-
      binding DSL: sampler@0 + UBO@1, pipeline layout reuses
      `RenderPushConsts`, single fixed `(Over, BGRA8)` pipeline).
      New `convolution.frag.glsl` (scalar block layout, loops over
      `dims.x × dims.y` weights).
    - T2: build.rs auto-picks up the new `.glsl` — no code change.
    - T3: kernel UBO uses per-draw `BatchUploadArena` slice
      (deviation from plan's persistent buffer — avoids the WAW
      hazard between back-to-back convolution composites in one
      paint batch). `BatchUploadArena` gained `UNIFORM_BUFFER`
      usage; new `KERNEL_UBO_ALIGNMENT = 256` const satisfies every
      reported `minUniformBufferOffsetAlignment` in the wild.
    - T4: `try_vk_render_composite` early-dispatches to a new
      `try_vk_render_composite_convolution` helper when src is
      Drawable + filter is Convolution + op == Over + no mask +
      identity transform + Repeat::None + dst is BGRA. Out-of-range
      kernel / wrong dst format returns `None` so the caller falls
      through to the standard LINEAR-sampler path (silent kernel-
      ignore, matches pre-phase-2 behaviour).
    - T5: 7 unit tests for `ConvolutionKernelData` (size, byte
      layout, validation) + an `#[ignore]` integration test
      (`tests/convolution_pipeline_smoke.rs`) that builds the
      pipeline against a real VkContext and roundtrips a 5×5
      box-blur kernel through a `BatchUploadArena` UBO slot.
      All 163 lib tests + the convolution integration test PASS
      under the sandbox's lavapipe ICD.
    - **Hardware smoke outcome (2026-05-15):** xfwm4 / marco /
      xfwm4-shadow use **pre-rendered alpha pixmaps** for shadows,
      not RENDER convolution — confirmed by 89 SetPictureFilter
      calls in a fresh xfwm4 run, all `nearest`. picom v13 with
      `xrender` + kernel blur is the canonical RENDER convolution
      consumer; under `just yserver-picom-hw` picom imports
      windows + screen-redirects but then only runs
      `_draw_callback` once and goes silent — likely a Damage /
      XFixes infrastructure gap on yserver that blocks per-Damage
      recompositing. No filter calls reached yserver from picom.
    - **Independent fix landed alongside (uncommitted):**
      `write_render_query_filters_reply` in
      `yserver-protocol/src/x11/mod.rs` now advertises the
      standard X.Org filter list (was empty: 0 filters / 0
      aliases). Clients that consult QueryFilters before issuing
      SetPictureFilter (picom does, xfwm4 doesn't) will now find
      `convolution` in the advertised set.
    - **Picom validation harness landed (uncommitted):**
      `tools/picom-yserver.sh` + `just yserver-picom-hw` — writes
      a temp `picom.conf` with `xrender + kernel + 9x9gaussian +
      inactive-opacity 0.7`, runs picom against yserver. Useful
      when the Damage/XFixes gap is fixed and we want to validate
      the convolution path against a real consumer.
    - The unrelated **black bars around xfce pop-up menus** that
      motivated the smoke turned out to be `NameWindowPixmap`
      returning `BadAlloc` under Manual-mode redirect, not a
      missing filter — diagnosis logged at
      `[[feedback-namewindowpixmap-badalloc]]` and in
      `known-issues.md`. xfwm4 falls back to compositing the live
      window, which only has visible RGB — GTK CSD shadow-alpha
      in the offscreen backing is unreachable, so xfwm4 emits
      opaque shadow-color pixels where the alpha gradient should
      fade.

### Remaining — in priority order

- [ ] **Phase 6 — Resource lifetime: batch-owned refcounted handles**
  - Codex's long-term recommendation from 3B salvage: instead of relying on protocol destruction barriers + `queue_wait_idle`, adopt destroyed VkImages into the open `PaintBatch` via `BatchResource` so destruction defers automatically.
  - Subsumes the 3D needs_grow + pre-resize-flush pattern for `CopyScratch`, the analogous patterns 3F introduced for `MaskScratch` + `dst_readback`, the 3B destruction-barrier collection, the Phase-5 `Retired*Image` flavours, and the pixmap-pool `PooledPixmapReturn` — all into a uniform refcounted-handle model.
- [ ] **AMD-specific investigation — STILL OUTSTANDING after pool + GPU-trap + diag-followups**
  - Smoke on 2026-05-14: bee at `pre-timeline-rework` (= post-pool + post-GPU-trap + gradient-flush-removed) is **no GPU faults but laggy**. The pool kernel-allocator + GPU-trap wins didn't close the perceived lag gap on RDNA2; they did on fuji/Intel.
  - Telemetry showed `queue_submit2` peaks ~1000-1200/sec on bee during MATE drag. Per-source split: `picture_destroy` ~975/sec, `one_shot` ~695/sec, `other (init_clear)` ~140/sec, `compositor` ~60/sec, paint paths the rest. The picture_destroy contribution is the natural next target (timeline-migration would have closed it if not for the bee-specific UAF problem above).
  - Pre-existing perf snapshot showed 42.86% of CPU in `libvulkan_radeon.so` (RADV per-submit cost) vs 25.54% in yserver. So even AT the same submit rate, bee carries a substantially higher per-submit driver tax than fuji.
  - Next investigation moves (when resumed): (a) retry timeline-semaphore migration with compositor-side annotation included, (b) reduce the absolute number of one_shot submits (glyph atlas + gradient creates), (c) ftrace amdgpu BO create/submit/wait rates per `project_amd_lag_investigation.md`.

---

## Followups not on the rework critical path

See `known-issues.md` for the full ticklist with detail. Highlights tracked here for awareness during rework planning:

- [ ] **`disable_output` atomic EINVAL** — recurring shutdown warn; disarm path mitigates but per-property split is the real fix.
- [x] **Composite Manual-mode (`update=1`) accepted** — fixed 2026-05-14: both modes register the redirect record; `activate_redirect_backing_for` is skipped (decoupling the record from backing allocation). Unblocks notification-area-applet, xfwm4, picom, xcompmgr. Hardware-confirmed on fuji.
- [x] **GTK file-manager popup offset** — fixed 2026-05-15 (`ea7c186`): XIQueryPointer reply put `same_screen` in `pad0`; correcting the field layout fixed Caja + Thunar.
- [x] **Caja wheel needs view-switch** — largely fixed 2026-05-15 (`5583def`, `eb0bc12`): wired XI2 valuator scroll end-to-end (XIScrollClass + scroll valuators 2/3, XI_Motion-with-scroll-axis emission, per-event delta in Relative mode). Residual non-deterministic warm-up on the first GTK app per session filed in known-issues.
- [x] **Color R↔B swap on JPEG backgrounds + saturated icons** — fixed 2026-05-14: `try_vk_put_image` now treats the wire as `[B,G,R,A]` per the advertised visual masks (matches `B8G8R8A8_UNORM`). Hardware-confirmed on fuji.
- [x] **`minor_code = 0` hardcoded in extension error encoder** — fixed 2026-05-15: 76 extension-dispatcher call sites converted to `emit_x11_error_with_minor`. Composite, MIT-SHM (+ children), PRESENT, DRI3, GLX, RANDR, XI2/XKEYBOARD, RENDER now emit the real per-extension minor; core requests stay on `emit_x11_error` (minor=0 is spec-correct).
- [~] **Listener starvation under chatty clients** — not reproducible as of 2026-05-15. Single-threaded core loop's per-iteration read budget is still unbounded, so the underlying shape remains; revisit if a new starvation case shows up.
- [x] **xfce4 text rendering** — fixed (3E text-run migration + downstream rework). Confirmed 2026-05-15.
- [x] **XInput2 valuator scroll** — implemented 2026-05-15 (`5583def`, `eb0bc12`). See "Caja wheel needs view-switch" entry above.
- [ ] **Per-glyph queue_wait_idle in `GlyphAtlas::intern`** — phase-5 scope but called out so 3E results aren't read as "text path is fully batched."

---

## Source-of-truth pointers

- HLD: `docs/superpowers/specs/2026-05-12-rendering-rearchitecture.md`
- Phase plans: `docs/superpowers/plans/2026-05-1[23]-rendering-rearchitecture-phase3{a,b,c,d,e,f}.md`
- Phase results: same directory, `*-results.md` suffix.
- Cross-cutting bugs: `known-issues.md`
- Pre-rework history: `status-archive-2026-05-13.md`
- Per-skill memory: `~/.claude/projects/-home-jos-Projects-yserver/memory/MEMORY.md`
