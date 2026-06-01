# Idle-vblank MSC pacing design

**Status:** Draft, 2026-06-01 (awaiting codex review).
**Branch (planned):** `fix/present-vblank-msc-rebase` (completes the existing per-CRTC vblank-paced Present work in commit `698e112`; lands together with it).
**Related:**
- `crates/yserver/src/drm/page_flip.rs` — `request_next_vblank_event` (the broken legacy path), `dispatch_event` (the event drain).
- `crates/yserver-core/src/core_loop/run.rs` — `drain_present_completions` (the idle-advance call site).
- `crates/yserver/src/kms/v2/backend.rs` — `request_next_vblank_event` backend impl, `record_crtc_ust_msc`, `present_get_ust_msc`, `vblank_request_in_flight`.
- `xserver/hw/xfree86/drivers/modesetting/{drmmode_display.c,present.c}` — upstream reference: `ms_queue_vblank` uses `drmCrtcQueueSequence`, falls back to `drmWaitVBlank` only on old kernels.
- Diagnosis: `docs/status.md` → "Cinnamon keyring modal freezes the shell clock — DIAGNOSED (2026-06-01)".

## Goal

Make the Present MSC advance at the display refresh rate **even when the scene is static** (no client-driven pageflips), so a client that paces its frame clock on `PresentNotifyMSC` / `loader_dri3_wait_for_msc` (Cinnamon's Cogl under a full-screen-composited modal) is woken every vblank instead of ~once per second. Concretely: the Cinnamon keyring modal must keep the panel clock ticking and stay click/type-responsive.

## Background

### What 698e112 already gives us

Commit `698e112` ("per-CRTC vblank-paced Present completion", rebased from `fb89605`) replaced the old synchronous fire-on-submit `(0,0)` Present completion with Xorg-style queue-then-fire-on-vblank:

- `PresentPixmap` / `PresentNotifyMSC` enqueue a `PendingCompleteNotify` into `state.pending_complete_notify` instead of completing immediately (IdleNotify still fires immediately).
- On each retired pageflip, `record_crtc_ust_msc` pushes `(msc, ust)` to `recent_page_flips`; `run.rs::drain_present_completions` drains them and fires the queued completions with real kernel `(msc, ust)` via `drain_pending_complete_notify_for_flip`.
- `has_vblank_pacing()` gates this path on for the KMS backend.
- A `present_get_ust_msc(_window)` Backend hook surfaces the current `(msc, ust)` (currently hardcodes output 0).

This is the correct architecture and **is not on master** — master still does fire-on-submit `(0,0)`. The bug below is the one defective piece of it.

### The bug (timing-proven)

When no client is submitting frames, there are no pageflips, so `recent_page_flips` stays empty and the queued completions never fire from real flips. 698e112's fallback for this is the **"T6 idle-case MSC advance"**: when `pending_complete_notify` is non-empty, `run.rs` calls `backend.request_next_vblank_event()`, which calls `drm/page_flip.rs::request_next_vblank_event(device, crtc_index=0)`:

```rust
device.wait_vblank(VblankWaitTarget::Relative(1), VblankWaitFlags::EVENT, crtc_index /*=0*/, 0)
```

This is **legacy `drmWaitVBlank`**. Under amdgpu **atomic** KMS, when no flip stream is active the per-CRTC vblank IRQ is gated, and a legacy relative-`drmWaitVBlank` does **not** reliably arm a one-shot at the next vblank — it is serviced ~1 second late. Measured on silence (rx580/RADV), instrumented:

```
request_next_vblank_event -> armed=true
prior  Vblank  msc=1191533  t=19888.582571s
armed  Vblank  msc=1191591  t=19889.550045s   = +967ms, +58 msc
```

A 1-second stall window contains only ~2 vblank events instead of 60. The run loop is alive (composites throughout the gap), so it is the **kernel not delivering the armed vblank** — not a slow loop. Net effect: MSC advances ~1/sec → Cogl frame clock runs at ~1 fps → panel clock frozen, modal starved.

This is screen-count-independent (reproduces single- and dual-monitor) and Cinnamon-specific (Cogl paces on MSC; marco/MATE do not). It matches the prior codex review note that the idle advance "sourced the number from the wrong thing, not the actual vblank."

## Non-goals

- Changing the deferred-completion / `pending_complete_notify` architecture itself — it is correct; we only fix how MSC advances when idle.
- A software/wall-clock MSC estimator (faking `(msc, ust)` from elapsed time). Rejected: non-monotonic vs real flips, and clients compare our MSC against DRI3 buffer fences.
- Continuous "null" pageflips (re-flipping the current FB every vblank to keep the IRQ hot). Rejected: wasteful, fights cursor/atomic commits, and still depends on the flip cadence.
- Fixing the dual-monitor non-monotonic MSC artifact (CRTC 63 vs 66 counters ~6857 apart) beyond what per-window CRTC binding gives us for free. It is secondary; track separately if it survives.
- DPMS-off / suspended-seat behavior — `scanout_allowed()` already gates those (the queue must be a no-op when not Active, same as today).

## Approach

Mirror the modesetting driver: arm vblanks with **`DRM_IOCTL_CRTC_QUEUE_SEQUENCE`** on the real CRTC, with `DRM_CRTC_SEQUENCE_NEXT_ON_MISS` so a target that has already passed fires at the next vblank instead of waiting a full wrap. Parse the resulting `DRM_EVENT_CRTC_SEQUENCE` completion ourselves (the `drm` 0.15 crate does not decode it). Bind each Present waiter to a single CRTC so we arm the right pipe and report a monotonic MSC.

Three parts, all required:

### Part A — arm via `drmCrtcQueueSequence`

`drm-sys` 0.8.1 exposes the primitives; the high-level `drm` 0.15 crate and `drm-ffi` 0.9.1 do **not** wrap this ioctl, so we issue it raw.

```
struct drm_crtc_queue_sequence { crtc_id: u32, flags: u32, sequence: u64, user_data: u64 }
DRM_CRTC_SEQUENCE_RELATIVE     = 1
DRM_CRTC_SEQUENCE_NEXT_ON_MISS = 2
ioctl = _IOWR('d' /*0x64*/, 0x3C, drm_crtc_queue_sequence)   // DRM_IOCTL_CRTC_QUEUE_SEQUENCE
```

New helper in `drm/page_flip.rs`:

```rust
/// Queue a one-shot CRTC sequence (vblank) event. `crtc_id` is the raw
/// KMS CRTC object id (NOT a pipe index). With RELATIVE the kernel fires
/// `sequence` vblanks from now; NEXT_ON_MISS guarantees delivery at the
/// next vblank if the absolute target already elapsed. The completion
/// arrives as DRM_EVENT_CRTC_SEQUENCE carrying `user_data` verbatim.
pub fn queue_crtc_sequence(
    device: &Device,
    crtc_id: u32,
    relative: bool,
    sequence: u64,
    user_data: u64,
) -> io::Result<u64> /* kernel-returned scheduled sequence */
```

Issued via `rustix::ioctl` (or `nix`) with the computed request code. Two arming strategies (recommend the first):

1. **Per-target absolute (Xorg-correct, preferred):** for each distinct pending completion `target_msc` on a CRTC, arm `queue_crtc_sequence(crtc, relative=false, sequence=target_msc, …)` once (deduped by `(crtc, target_msc)`). A `target_msc==0` ("next") arms `relative=true, sequence=1`. The kernel fires exactly when MSC reaches the target. No per-vblank wakeups.
2. **Relative keep-alive (simpler fallback):** while any waiter is pending, keep one `relative=true, sequence=1` armed per active CRTC, re-arming on each completion. Drains whatever is due each vblank. More wakeups but trivially correct.

`request_next_vblank_event` is generalized to take the target CRTC (and, for strategy 1, the target sequence). The `vblank_request_in_flight: bool` dedup becomes per-CRTC (`HashMap<crtc, in_flight>` or per-`(crtc, target)` for strategy 1) so arming one CRTC doesn't suppress another.

### Part B — parse the `DRM_EVENT_CRTC_SEQUENCE` completion

`drm` 0.15's `Event` enum decodes only `DRM_EVENT_VBLANK` and `DRM_EVENT_FLIP_COMPLETE`; everything else becomes `Event::Unknown(raw_bytes)`, which `dispatch_event` currently drops. Add a parse arm:

```
struct drm_event_crtc_sequence { base: drm_event{type_:u32,length:u32}, user_data:u64, time_ns:i64, sequence:u64 }
DRM_EVENT_CRTC_SEQUENCE = 3
```

In `dispatch_event`, when `Event::Unknown(bytes)` has `bytes[0..4] as u32 == DRM_EVENT_CRTC_SEQUENCE`, decode `(sequence, time_ns, user_data)` and route to the same `on_page_flip(crtc, msc, ust)` callback Vblank uses:

- `msc = sequence` (widen u64; per-CRTC wrap bookkeeping already lives in the backend).
- `ust = Duration::from_nanos(time_ns as u64)` — note this is **CLOCK_MONOTONIC ns**, the same clock as `PageFlipEvent.duration`/`VblankEvent.time`, so it composes with the existing `(msc, ust)` consumers.
- **`crtc` must come from `user_data`** — the sequence event has **no `crtc` field** (unlike `VblankEvent`). So Part A must encode the output index / crtc id into `user_data` when queuing, and Part B decodes it. Proposed: `user_data = output_idx as u64` (small, stable, maps back to `Output`).

`record_crtc_ust_msc(output_idx, msc, ust)` then clears the per-CRTC in-flight flag and pushes to `recent_page_flips` exactly as a pageflip does, so `drain_present_completions` fires the queued completions unchanged.

### Part C — bind the Present waiter to one CRTC

`present_get_ust_msc(_window)` currently hardcodes output 0, and `drain_pending_complete_notify_for_flip` fires every pending completion with whichever CRTC retired. For a window spanning multiple outputs this mixes counters (the 6857-offset artifact). Bind each presented window to a single CRTC:

- Pick the CRTC of the output the window most covers (max-intersection-area), falling back to primary. Cache on the per-window Present record.
- Arm idle vblanks (Part A) on that CRTC, and fire that window's completions only from that CRTC's `(msc, ust)`. This yields a monotonic MSC for the window and arms the correct pipe.

A single-output session collapses to "always the one CRTC" — no behavior change there, so this does not risk the common case.

## Architecture summary

```
client PresentNotifyMSC/PresentPixmap(target_msc=T, window=W)
  └─ enqueue PendingCompleteNotify{W, target=T, crtc = pick_crtc(W)}        (existing, + crtc binding)

run.rs::drain_present_completions (each loop iter, has_vblank_pacing):
  1. drain real pageflip retires → fire due completions          (existing)
  2. for each pending completion not yet armed:
       backend.arm_idle_vblank(crtc, target)                     (NEW: Part A/C)
          └─ queue_crtc_sequence(device, crtc_id, abs target, user_data=output_idx)

drm fd readable → drain_events → dispatch_event:
   Event::PageFlip / Event::Vblank          → on_page_flip(crtc,msc,ust)    (existing)
   Event::Unknown(type==CRTC_SEQUENCE)      → decode → on_page_flip(crtc,msc,ust)  (NEW: Part B)
      └─ record_crtc_ust_msc(output_idx, msc, ust) → recent_page_flips, clear in-flight
   → next drain_present_completions fires the queued completion with real (msc,ust)
```

## Testing

### Unit (yserver-core / yserver, no DRM)
- `drm_event_crtc_sequence` byte-parse round-trip in `dispatch_event` (construct raw bytes, assert decoded `(crtc via user_data, msc, ust)` → callback), mirroring the existing `dispatch_event_surfaces_vblank_event` test.
- `pick_crtc(window)` max-coverage selection (single-output → that output; dual-output → larger intersection; off-screen → primary).
- Per-CRTC in-flight dedup: arming CRTC A does not suppress arming CRTC B; re-arm after completion.

### Integration
- Existing 518-test suite stays green; `present_get_ust_msc` per-window tests updated for the CRTC binding.

### Manual hardware (REQUIRED before commit — see Risks)
1. **Cinnamon keyring (the bug):** on silence, `just yserver-cinnamon-hw` (direct, no x11trace). Trigger the keyring/polkit modal. PASS = panel clock keeps ticking while the modal is up AND the modal dismisses on the first click / accepts typing immediately. Confirm with `PRESENT-DBG`: idle `CRTC_SEQUENCE` events arrive at ~refresh rate (Δmsc≈1 per ~16ms), not +58/sec.
2. **No black-scanout regression** (the shelved `feature/deferred-present-completion` failure mode): MATE and a normal compositing session (xfce/picom) must render — not cursor-on-black. Test on **bee/RADV** specifically (where that regression was originally seen) plus silence.
3. **Idle CPU:** with strategy 1 (per-target absolute), an idle desktop must not spin arming vblanks every iteration; confirm idle CPU is comparable to pre-fix. (If strategy 2 is chosen, document the per-vblank wakeup cost.)
4. **VT switch / DPMS:** arming must be a no-op when `!scanout_allowed()`; switch away/back and DPMS off/on must not leak armed sequences or wedge.
5. Matrix smoke: bee, silence, yoga (rate canary).

## Risks

- **Black-scanout regression.** present-vblank-msc was never HW-validated; its bigger sibling `feature/deferred-present-completion` was shelved for an empty-scanout BO → black screen on bee/RADV. This fix makes the deferred path *actually fire continuously*, so it exercises that path harder. Mitigation: HW test #2 is a hard gate; keep `PRESENT-DBG` until verified.
- **`user_data`-carries-CRTC.** Forgetting that the sequence event has no `crtc` field (encoding output_idx into `user_data`) would misroute completions. Covered by the unit round-trip test.
- **Sequence wrap.** Kernel sequence is u64 in the event but the hardware counter is 32-bit; reuse the backend's existing per-CRTC wrap bookkeeping; do not re-widen.
- **Raw ioctl correctness.** `_IOWR('d', 0x3C, drm_crtc_queue_sequence)` size/dir must match the kernel struct exactly; verify against `<drm/drm.h>` on the test kernel. EOPNOTSUPP on very old kernels → fall back to the legacy path (and log once) rather than failing the present.
- **Per-CRTC in-flight state** replaces a single bool; ensure it is cleared on the `CRTC_SEQUENCE` completion AND on real pageflip retires (both advance MSC), and reset across VT suspend/resume.

## Rollout

1. Implement Parts A–C on `fix/present-vblank-msc-rebase` (on top of `698e112`).
2. `cargo fmt` / `cargo clippy -- -W clippy::pedantic` / `cargo test` green.
3. HW smoke (all five checks above) — user-observed, per the "no commit before HW smoke" rule.
4. Strip/demote `PRESENT-DBG` instrumentation.
5. Squash `698e112` + the fix into a coherent "per-CRTC vblank-paced Present completion (with atomic idle-vblank pacing)" change and merge to master.

## Follow-ups

- Dual-monitor non-monotonic MSC: confirm the per-window CRTC binding fully resolves it; if a window genuinely spans two CRTCs with skewed counters, decide a policy (report the bound CRTC's MSC only).
- Fold `present_get_ust_msc`'s remaining single-output assumptions once the CRTC binding is the single source of truth.
