# Fix: honor IMPLICIT dma-buf sync on DRI3-imported pixmaps (Firefox HW-WebRender intermittent blank)

> **Review status: GO** — codex-reviewed over 3 rounds (root cause + Xwayland
> contract validated; round-1 fence-direction/lifetime fixes; round-2 batching
> blocker → wait accumulator; round-3 GO). Safe to implement as written.
>
> **LOAD-BEARING INVARIANT** the accumulator relies on: **one open frame ==
> one `vkQueueSubmit2`** (verified: `backend.copy_area` never submits itself;
> the frame closes once via `close_open_frame → flush_submit_group`). The
> "import the producer sync_file ONCE, share the binary semaphore across all
> sub-rects" rule is correct ONLY under this invariant (a binary sync_file
> semaphore is single-use). If a future change ever splits one open frame
> across multiple submits, this breaks — re-export per submit (or switch to a
> timeline semaphore) at that point. Add a debug-assert / comment at the
> accumulator so this invariant is not silently violated later.

## Status of prior hypothesis
A previous draft (`2026-05-29-present-wait-fence.md`, now SUPERSEDED) blamed
PresentPixmap ignoring `wait_fence`. **Instrumentation refuted it:** on a
blank Firefox run, all 3056 presents had `wait_fence == 0` (logged
`wait_triggered=no-fence`). No present carries an explicit fence, so there is
nothing to wait on there.

## Symptom (recap)
Firefox HW WebRender (EGL+dmabuf, RX580/radeonsi, in-process compositor)
renders **intermittently blank** under yserver (stale wallpaper). SW render
always works; glxgears works; correct & deterministic on Xorg. Outcome flips
with pure timing perturbations (stdout→file, `MOZ_LOG=,sync,`, initial page).

## Root cause (data + code)
- **Data:** every present uses **implicit sync** (`wait_fence == 0`).
  Content-sized presents DO arrive (e.g. `1136x1009`, plus the compositor's
  `5120x1440`).
- **Code:** `crates/yserver/src/kms/vk/target.rs::from_dmabuf` imports the
  client dma-buf into a `VkImage` and does **no implicit-fence handling**.
  There is **no `DMA_BUF_IOCTL_EXPORT_SYNC_FILE`** anywhere in the tree. So
  when `backend.copy_area` (and/or the compose/scanout sample) reads the
  imported dma-buf, it does **not wait for the producer's (Firefox's GPU)
  implicit fence**. yserver reads while the GPU may still be writing →
  intermittent blank/stale/torn.

Every X server that consumes DRI3 dma-buf pixmaps with a Vulkan/GL backend
must participate in implicit sync (Vulkan does NOT do it automatically). Xorg
+ glamor/modesetting does; that's why it's correct on Xorg.

## The contract (implicit dma-buf sync, Vulkan consumer)
Implicit sync lives on the dma-buf's `reservation_object`. A Vulkan importer
must bridge it explicitly:
- **Before READING** the buffer: export the buffer's current implicit fence
  as a sync_file — `ioctl(dmabuf_fd, DMA_BUF_IOCTL_EXPORT_SYNC_FILE,
  { flags: DMA_BUF_SYNC_READ, fd: <out> })` — import it as a `VkSemaphore`
  (we already have `kms/vk/sync.rs::import_sync_file`), and **wait** on that
  semaphore in the copy/compose submission.
- **After our READ** (for correct buffer reuse / ping-pong): export OUR read-
  completion fence as a sync_file and attach it back to the dma-buf —
  `ioctl(dmabuf_fd, DMA_BUF_IOCTL_IMPORT_SYNC_FILE, { flags:
  DMA_BUF_SYNC_WRITE, fd: <our fence sync_file> })` — so the next WRITER
  (Mesa) waits for our read before overwriting. (`export_sync_file` exists.)
  **Codex-corrected: the import-back flag is `DMA_BUF_SYNC_WRITE`, NOT
  `_READ`** — verified against Xwayland (`xwayland-glamor-gbm.c:955`,
  `xwayland-present.c:900`), which exports the implicit fence and imports it
  back with WRITE. **This is NOT optional** for correctness when the producer
  reuses the buffer (it does); skipping it allows producer-overwrite mid-read
  → tearing. (Staging it after the read-side fix is fine as an implementation
  *sequence*, but it must land for a correct dma-buf contract.)
  REFERENCE IMPL to mirror: Xwayland `xwl_glamor_dmabuf_import_sync_file` +
  the present path in ../xserver/hw/xwayland/.
- The implicit fence **advances per GPU write**, so the EXPORT must be done
  **fresh at each present/copy**, not once at import.

## Design

### Retain the dma-buf fd per imported pixmap
Currently `from_dmabuf` consumes the fd into the VkImage import. Add a `dup()`
of the dma-buf fd, stored alongside the imported pixmap's backend record (the
DrawableStore entry / DRI3 sync-resources map), so it's reachable at present
time for the per-present EXPORT/IMPORT ioctls. Close on pixmap free.

### Gate the copy on the producer fence (READ side — the actual bug)
In the present/copy path (`copy_area` from an imported-dmabuf source), if the
source pixmap is dma-buf-backed:
1. EXPORT_SYNC_FILE (READ) from the retained fd → sync_file fd.
2. `import_sync_file` → `VkSemaphore`.
3. Add it as a **wait semaphore** on the copy submission (wait at
   `TRANSFER`/`ALL_COMMANDS`). The GPU queue waits — **the CPU core loop does
   NOT block**, so this is inherently single-core-safe (unlike the refuted
   wait_fence/CPU-wait approach).
4. **Retain the semaphore until the submission RETIRES** (wrap in
   `OwnedSemaphore`/`Arc` like `dri3_fence_from_fd` does, tied to the batch
   ticket) — do NOT destroy right after `vkQueueSubmit2` returns. (Codex:
   destroying immediately is a use-after-free of GPU-referenced semaphore.)
If EXPORT_SYNC_FILE returns no fence (buffer idle) → skip (nothing to wait).

### REQUIRED PLUMBING — the submit path carries only SIGNAL semaphores today
**Codex blocker: there is no place to attach a WAIT semaphore at the current
`copy_area` API level.** `PresentPixmap` calls `backend.copy_area(...)`
(process_request.rs:5334), but the Vulkan submit path only carries signal
semaphores: `SubmitGroup::GroupEntry` has only `signal`
(kms/v2/submit_group.rs:34/37); `submit_paint_cb_with_semaphore` accepts only a
completion signal (kms/v2/platform.rs:1447); `flush_submit_group` builds only
`signal_semaphore_infos` (kms/v2/platform.rs:1595). So we must thread a
wait-semaphore path down to `vkQueueSubmit2`'s `wait_semaphore_infos`. This is
the load-bearing change; the ioctl/import is the easy part.

#### Wait ACCUMULATOR (codex round-2 blocker — a scalar `GroupEntry.wait` is NOT enough)
The call chain FANS OUT: one `PresentPixmap` → N `backend.copy_area` (one per
update rect, process_request.rs:5331) → each → M `engine.copy_area` (one per
ClipByChildren sub-rect, backend.rs:8453/8588). Many sub-rect CBs — and
possibly **multiple presents from different source buffers** — coalesce into
ONE open frame / one `vkQueueSubmit2`. So the design needs a **per-open-frame
wait accumulator**, not a single fence slot:
- Maintain a set on the open frame: `wait: HashMap<DrawableId,
  Arc<OwnedSemaphore>>` keyed by **source drawable** (the imported dma-buf
  pixmap).
- The FIRST `engine.copy_area` that reads a given imported dma-buf source in
  this frame does the `EXPORT_SYNC_FILE(READ)` + `import_sync_file` ONCE and
  inserts the `Arc<OwnedSemaphore>` under that source id. Subsequent sub-rects
  reading the same source in the same submit reuse it (do NOT re-export).
  **Rationale: a sync_file-imported binary `VkSemaphore` is single-use** — one
  `vkQueueSubmit2` waits on it once before all its CBs run, which correctly
  gates every sub-rect copy of that source. Re-exporting per sub-rect would be
  wrong (multiple binary waits on the same payload) and wasteful.
- At `flush_submit_group` (frame close), pass the accumulated set's semaphores
  as `wait_semaphore_infos` (stage `TRANSFER`/`ALL_COMMANDS`) to
  `vkQueueSubmit2`. Different source buffers each contribute one wait.
- Retain each `Arc<OwnedSemaphore>` on the batch ticket until the submission
  RETIRES (same retirement pin as the signal/export semaphores).
- A fresh export happens **next frame** (new open frame → empty accumulator),
  matching "implicit fence advances per write".
So "plumbing first" = add the per-frame wait accumulator AND the
`GroupEntry`/`flush_submit_group` `wait_semaphore_infos` wiring; only then do
the later steps have somewhere to deposit the fence.

### Attach our read fence back (WRITE-after-READ hazard — do after read side proven)
After the copy submission, export our completion semaphore as a sync_file and
IMPORT_SYNC_FILE (READ) it onto the dma-buf so Mesa serializes its next write.
Land this as a second step once the read-side fix is shown to stop the blank.

### Compose/scanout path — NOT the current bug (codex), deprioritize
Codex confirmed DRI3 imports are stored `scene_participating = false`
(kms/v2/backend.rs:11265) and the scene compositor only emits
scene-participating drawables (gate at kms/v2/scene.rs:1750-1752). So the
compose/scanout tick does NOT directly sample the imported pixmap today — the
live consumer is `PresentPixmap → copy_area`. Keep this audit but it's lower
priority and not on the Firefox-blank path.

### FreePixmap mid-present
Codex: already covered by the store's refcount/ticket retirement
(kms/v2/store.rs:688,748). The retained dma-buf fd must be closed on actual
drawable destruction (retirement), not at request end.

## Why this fits everything
- Timing-sensitivity: a producer-not-done race; delays (stdout/sync) let the
  GPU finish first. ✔
- Xorg correct: honors implicit sync. ✔
- SW render works: no dma-buf, CPU buffer ready synchronously. ✔
- glxgears works: simple/fast GPU work usually done before our read (and/or
  buffer reuse pattern hides it). ✔
- first-frame stale wallpaper: read an un-written buffer. ✔

## Open questions for review (codex)
1. Is EXPORT_SYNC_FILE(READ)→VkSemaphore-wait the right Vulkan-consumer
   bridge for implicit dma-buf sync, matching how Xorg/Mesa expect a consumer
   to behave? Any modifier/DCC caveat on radeonsi (Polaris) for sync_file
   export?
2. Is dup-and-retain of the dma-buf fd the right way to keep the implicit
   fence reachable per-present, or is there a Vulkan-native way (e.g. the
   VkImage/VkDeviceMemory already lets us export the implicit fence without
   the raw fd)? Lifetime/leak concerns.
3. Read-fence-back (IMPORT_SYNC_FILE after our read): required for
   correctness, or does Mesa's own sync handle producer-side serialization
   for the X11 present copy case? Risk of tearing without it.
4. Is the copy submission the only consumer, or does compose/scanout sample
   imported dma-bufs directly (needing the same gate)?
5. Per-present EXPORT_SYNC_FILE ioctl + transient semaphore import/destroy
   cost — acceptable per present, or should semaphores be pooled?
6. Anything simpler/more-correct: e.g. negotiating EXPLICIT sync (DRI3 1.4
   syncobj / PresentPixmapSynced) so Mesa hands us fences instead — but that's
   opt-in and we must honor implicit anyway as the baseline.

## Test / verify
- HW smoke: Firefox HW WebRender on `:7` renders real pages reliably across
  repeated launches (no stdout redirect, no SW flags); intermittent blank
  gone. (no-commit-before-smoke.)
- Regression: glxgears, vscode, MATE compose; xts present coverage.
- Keep PRESENT-INSTR + the fb8d388 Present-drop WARN during bring-up.
