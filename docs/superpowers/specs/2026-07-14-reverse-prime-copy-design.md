# Reverse-PRIME copy scanout design

## Goal

Add a compatibility scanout mechanism for a render GPU A and an output/KMS
GPU B when neither copy-free allocation direction works:

- output-owned: B allocates one buffer which A renders and B scans out;
- renderer-owned: A allocates one buffer which A renders and B scans out;
- copied: A renders an exportable source, then B copies it into an independent
  B-local scannable destination.

The copied path removes the requirement that one allocation be both writable
by A and scannable by B. B must still be able to import A's DMA-BUF as a Vulkan
copy source with its exact modifier and pitch. The Vulkan implementation
therefore requires `VK_EXT_image_drm_format_modifier` on B; sink Vulkan drivers
without explicit DMA-BUF layout import support are rejected before any
foreign-memory submission. CPU readback/upload is not part of this design.

## Selection model

Copy is a transport mechanism, not a third allocation owner. Per-output
scanout is represented by one ADT:

```text
OutputScanout
  Shared(SharedScanoutPool)       owner = Output | Renderer
  Copied(CopiedScanoutPool)
```

Cross-device route selection tries, in order:

1. output-owned shared scanout;
2. renderer-owned shared scanout;
3. copied scanout.

Local routes retain the existing renderer-owned path. Each candidate is
validated end to end before it is installed.

## Completion-fd boundary

GPU A's render completion is the userspace scheduling boundary. A signals an
exportable Vulkan binary semaphore and exports its payload as a Linux
`sync_file`. The per-frame fd is registered inside a stable backend-owned
readiness aggregator. The core loop polls only the aggregator because
`Backend::poll_fds()` is sampled once during startup.

The core loop dispatches readiness through a dedicated backend fd kind. The
backend drains ready job ids and starts the B-side copy. Job identity is
monotonic and paired with a stable output key; raw fd values and output-vector
indices are not identities.

Polling the fd only schedules the handoff. B still imports and waits on the
now-signalled sync payload in its copy submission so the external-memory
dependency is explicit.

## Frame sequence

```text
Idle
  -> RenderingOnA
  -> WaitingForRenderCompletion
  -> CopySubmittedOnB + KmsFlipPending
  -> OnScreen
  -> Idle
```

1. Acquire one A source and one B destination.
2. Render the full output on A and signal the source semaphore.
3. Export/register the render-completion fd and retain all frame pins.
4. On readiness, import that fd into B, submit a full-image copy, and signal a
   B export semaphore.
5. Export B completion and submit the destination framebuffer to KMS as
   `IN_FENCE_FD`.
6. The ordinary page-flip completion commits damage acknowledgements and
   recycles the previous destination. The source is retained until this event,
   which is necessarily after the B copy because KMS waited on its fence.

There is no successful-path second userspace wait for B. If B submission
succeeds but the atomic commit fails, the B completion is retained for bounded
asynchronous recovery before either image is reused.

## Resource model

`CopiedScanoutPool` owns:

- an A-side exportable render-source ring;
- B-side imported aliases of those sources, with only copy-source usage;
- a B-local scanout-destination ring with copy-destination usage and DRM
  framebuffer registrations;
- one long-lived B Vulkan transfer context per sink DRM device;
- per-slot command buffers, export semaphores, and explicit state.

The B transfer context requests only the features needed for external memory,
external semaphore fds, synchronization2, and image copy. It must not require
compositor-only features such as logic operations or dual-source blending.

Drop and reset order is load-bearing:

1. stop new submissions and unregister completion fds;
2. wait or drain accepted A/B queue work;
3. disable/reset KMS state;
4. destroy B imported source aliases and B destinations;
5. destroy A source allocations;
6. drop B transfer contexts.

## Probe

The copy candidate probe uses disposable Vulkan devices so a failed foreign
memory import or copy cannot poison either live logical device. It validates a
full three-slot pool and, for every slot:

- A export allocation and color-attachment rendering;
- B import with the exact modifier, offsets, and pitches;
- B-local destination allocation;
- a real A render-completion handoff and B image copy;
- atomic `TEST_ONLY` scanout of the destination framebuffer.

The probe returns the exact successful source and destination allocation plans
for replay by the live pool.

The sink capability gate runs before pool allocation. Without
`VK_EXT_image_drm_format_modifier`, Vulkan's linear-image import chooses its own
row pitch and cannot represent the renderer's exported layout. Even when that
driver-chosen pitch happens to match, attempting the foreign import is not a
safe compatibility probe on affected older GPUs.

## Correctness and failure invariants

- At most one frame per output is waiting for A completion or pending a KMS
  flip.
- A source is never rendered again while B may still read it.
- A B destination is never written while pending or on screen.
- Damage, generation, cursor transition, and descriptor-pool pins advance only
  after page-flip completion.
- Completion registration failure does not block the core; it enters a bounded
  degraded poll path.
- A render failure returns both slots without creating an in-flight frame.
- B copy or atomic failure retains GPU-referenced resources until its fence is
  signalled, folds damage forward, and applies retry backoff.
- Connector disable, reprobe, VT release, and shutdown cancel or drain every
  waiting completion before destroying its resources.
- Device loss disables the affected copied route rather than reusing uncertain
  memory.
- A sink without explicit DMA-BUF layout import support rejects copied scanout
  before importing or submitting foreign GPU memory.

## Initial performance policy

The compatibility path renders and copies the full output. The current scene
renderer already forces full repaint, so this introduces no new buffer-age
correctness dependency. Damage-limited copies and immediate GPU-side A->B
semaphore chaining are later optimizations, gated on hardware telemetry.
