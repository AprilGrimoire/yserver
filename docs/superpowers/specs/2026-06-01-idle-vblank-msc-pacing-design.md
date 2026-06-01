# Idle-vblank MSC pacing design

**Status:** Ready to implement, 2026-06-01 (revision 3 — codex review converged over 2 rounds). Round 1 folded in: absolute queueing as the primary design; `user_data` carries stable `crtc_id` not `output_idx`; CRTC-scoped completion draining; checked `time_ns`; per-CRTC armed-target map with explicit suspend/hotplug reconciliation; side-effect-free sequence-event invariant. Round 2 closed the last gap: the malformed-`time_ns`/stale-`crtc_id` drop paths clear the armed-target entry *before* returning (unconditional clear-arm for any received `CRTC_SEQUENCE` on a known CRTC), so a dropped event can't strand a CRTC. Codex verdict: "implementable as written."
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

Issued via `rustix::ioctl` (or `nix`) with the computed request code.

**Arming is per-target ABSOLUTE — this is *the* design, matching Xorg.** This is not one of two co-equal options; the relative form is a narrow fallback (below). Xorg's modesetting Present path queues absolute MSCs (`ms_present_queue_vblank` → `MS_QUEUE_ABSOLUTE`, `present.c`) and its `ms_queue_vblank` helper *coalesces* requests and only re-arms when the wanted target differs from the one already in flight (`vblank.c`). We do the same:

- For each pending completion with `target_msc = T` bound to CRTC `C`, arm `queue_crtc_sequence(C, relative=false, sequence=T, flags=NEXT_ON_MISS, user_data=crtc_id(C))` **once**, deduped by `(C, T)`. The kernel fires exactly when `C`'s MSC reaches `T` — no per-vblank wakeups, low idle CPU.
- A `target_msc == 0` ("next vblank", what Cinnamon's `PresentNotifyMSC` sends) maps to `relative=true, sequence=1`.
- `NEXT_ON_MISS` only guards the already-passed case (fire at the next vblank instead of waiting a full counter wrap). **It must be paired with the `(C, T)` dedup**: never re-issue the same `(C, T)` while one is in flight, or an already-passed absolute target re-armed every loop iteration becomes a refire storm.

**Relative keep-alive is a fallback only**, used iff `DRM_IOCTL_CRTC_QUEUE_SEQUENCE` returns `EOPNOTSUPP` (pre-4.14-ish kernels): keep one `relative=1` armed per active CRTC, re-armed on each completion. Log once when taken. It is not the upstream behavior and not the low-idle-CPU path.

`request_next_vblank_event` is generalized to take the target CRTC + absolute target. Replace the single `vblank_request_in_flight: bool` with a **per-CRTC armed-target map** (`HashMap<crtc::Handle, u64 /*armed target*/>` or a small set of in-flight `(C, T)`), so arming one CRTC never suppresses another and we can coalesce. See "Lifecycle & reconciliation" for how this map is cleared.

### Part B — parse the `DRM_EVENT_CRTC_SEQUENCE` completion

`drm` 0.15's `Event` enum decodes only `DRM_EVENT_VBLANK` and `DRM_EVENT_FLIP_COMPLETE`; everything else becomes `Event::Unknown(raw_bytes)`, which `dispatch_event` currently drops. Add a parse arm:

```
struct drm_event_crtc_sequence { base: drm_event{type_:u32,length:u32}, user_data:u64, time_ns:i64, sequence:u64 }
DRM_EVENT_CRTC_SEQUENCE = 3
```

In `dispatch_event`, when `Event::Unknown(bytes)` has `bytes[0..4] as u32 == DRM_EVENT_CRTC_SEQUENCE` (and `length` matches `size_of::<drm_event_crtc_sequence>() == 24`), decode `(sequence, time_ns, user_data)` and route to the same `on_page_flip(crtc, msc, ust)` callback Vblank uses:

- **`crtc` comes from `user_data`, which carries the raw `crtc_id` — NOT an output index.** The sequence event has **no `crtc` field** (unlike `VblankEvent`), so Part A encodes the identity into `user_data`. It must be the *stable* `crtc_id` (KMS object id), because `output_idx` is **not stable**: `platform.rs` removes outputs by compacting the `Vec` (`platform.rs:2222`) and pageflip routing derives `output_idx` from the *current* Vec position on every event (`platform.rs:1197`/`:1203`). A `CRTC_SEQUENCE` event delayed across a hotplug / VT switch / mode change would otherwise resolve a stale `output_idx` to the wrong output. At completion, resolve `crtc_id → current output_idx`; **if no output currently owns that `crtc_id`, drop the event and clear that CRTC's armed-target entry** (do not fabricate an output).
- `msc = sequence` truncated to the hardware 32-bit counter and fed through the **existing** per-CRTC wrap bookkeeping in the backend — do not introduce a second widening of an already-widened value.
- `ust`: `time_ns` is **`i64`** (signed, CLOCK_MONOTONIC ns — same clock as `PageFlipEvent.duration`/`VblankEvent.time`). Convert with `u64::try_from(time_ns)`; on a negative/malformed value **log + skip the `(msc, ust)` update — but FIRST clear that CRTC's armed-target entry** (the sequence event proves the clock advanced, so the arm is spent; not clearing it would strand the CRTC in the dedup map = the same permanent-stall class as the old stuck bool). Then return. Do **not** `Duration::from_nanos(time_ns as u64)` (wraps a negative into a huge bogus duration). (`recent_page_flips` already truncates to µs at `backend.rs:2505-2507`; don't stack a signedness bug on the accepted precision loss.) Equivalently: clear-arm is unconditional for any received `CRTC_SEQUENCE` event on a known CRTC, *before* the `time_ns`/`crtc_id` validity checks that may drop it.

`record_crtc_ust_msc(output_idx, msc, ust)` then clears that CRTC's armed-target entry and pushes `(crtc/output_idx, msc, ust)` to `recent_page_flips` exactly as a pageflip does. **Invariant (black-scanout guard): the `CRTC_SEQUENCE` handler is side-effect-free except for MSC/UST accounting and clearing the armed-target entry — it must NOT touch scanout BO state, scene/BO-retire, or trigger a flip.** This keeps the new path orthogonal to the shelved deferred-present black-screen failure mode. A unit assertion guards it (see Testing).

### Part C — bind the Present waiter to one CRTC

`present_get_ust_msc(_window)` currently hardcodes output 0, and — more dangerously — `drain_pending_complete_notify_for_flip` (`process_request.rs:6973-6999`) fires every pending completion that matches `target_msc` **regardless of which CRTC produced the flip**. With two CRTCs on independent vblank clocks (measured 6857 apart on silence), a vblank from CRTC A can satisfy, by raw numeric MSC compare, a waiter that was armed against CRTC B — firing a completion with the wrong clock's `(msc, ust)`. This is a *correctness* bug, not just the dual-monitor "artifact"; it must be fixed as part of this change. Bind each presented window to a single CRTC and make the whole path CRTC-aware:

- **Pick a CRTC** for the window: the output it most covers (max-intersection-area), falling back to primary. Compute via `crtc_id`.
- **Store the bound CRTC on the `PendingCompleteNotify` entry itself**, not (only) a mutable per-window cache. A queued waiter must stay tied to the CRTC it was *armed against* even if the window moves/reconfigures before the delayed event returns.
- **Carry `crtc`/`output_idx` through `recent_page_flips`** (it becomes `Vec<(crtc, msc, ust)>`) and through `drain_pending_complete_notify_for_flip(state, crtc, msc, ust)`, which fires only the pending entries **bound to that same CRTC** (then `target_msc` compare within that CRTC). `present_get_ust_msc(window)` returns the bound CRTC's `(msc, ust)`. Both call sites in `run.rs` change: the paced per-flip loop (~line 770) passes each flip's `crtc`; the non-paced fallback (`run.rs:795`, RecordingBackend/HostX11, currently `(state, 0, 0)`) passes the single notional CRTC so non-KMS backends still fire all waiters. No call site keeps the old 3-arg form.
- Arm idle vblanks (Part A) on the bound CRTC's `crtc_id`.

A single-output session collapses to "always the one CRTC" — no behavior change there, so this does not risk the common case, while dual-output gets a monotonic per-window MSC.

### Lifecycle & reconciliation (in-flight armed-target map)

The per-CRTC armed-target map must never strand a CRTC with a stuck entry (a stuck entry → no re-arm → permanent ~0fps stall after the disturbance). Reconcile it at every event that can abort an armed sequence without delivering a completion:

- **`!scanout_allowed()` (VT suspend / master loss / DPMS off):** `request_next_vblank_event` must not just early-return — it must **clear the entire armed-target map** (the kernel drops queued sequences when we lose DRM master). On resume, the map is empty so `drain_present_completions` re-arms cleanly. Mirror this in the seat-suspend / DPMS-off paths so we don't depend on the next idle iteration.
- **Output removal / hotplug / reorder:** invalidate any armed entry whose `crtc_id` is no longer owned by a live output (the same compaction site, `platform.rs:2222-2230`). Because the map is keyed by `crtc::Handle`/`crtc_id` (stable), not by `output_idx`, a reorder alone doesn't corrupt it — but a removed CRTC's entry must be dropped.
- **Advance proof:** any `PageFlip`/`Vblank`/`CRTC_SEQUENCE` event for a CRTC clears that CRTC's armed entry (it proved the clock moved), exactly as `record_crtc_ust_msc` does today for the single bool.

## Architecture summary

```
client PresentNotifyMSC/PresentPixmap(target_msc=T, window=W)
  └─ enqueue PendingCompleteNotify{W, target=T, crtc = pick_crtc(W)}        (NEW: crtc bound ON the entry)

run.rs::drain_present_completions (each loop iter, has_vblank_pacing):
  1. drain real pageflip retires (crtc,msc,ust) → fire due completions BOUND TO THAT CRTC   (CRTC-scoped)
  2. for each pending completion whose (crtc,target) is not already armed:
       backend.arm_idle_vblank(crtc, target)                     (NEW: Part A/C, deduped by (crtc,target))
          └─ queue_crtc_sequence(device, crtc_id, ABSOLUTE target, NEXT_ON_MISS, user_data=crtc_id)

drm fd readable → drain_events → dispatch_event:
   Event::PageFlip / Event::Vblank          → on_page_flip(crtc,msc,ust)    (existing)
   Event::Unknown(type==CRTC_SEQUENCE,len==24) → decode (sequence,time_ns,user_data=crtc_id)  (NEW: Part B)
       resolve crtc_id→output_idx (drop+clear-arm if gone); ust=u64::try_from(time_ns)?
      └─ record_crtc_ust_msc(output_idx, msc, ust) → recent_page_flips{crtc}, clear (crtc,*) arm
         (side-effect-free: NO scanout BO / scene / flip mutation)
   → next drain_present_completions fires the queued completion for that crtc with real (msc,ust)
```

## Testing

### Unit (yserver-core / yserver, no DRM)
- `drm_event_crtc_sequence` byte-parse round-trip in `dispatch_event` (construct raw 24-byte event, assert decoded `(crtc_id via user_data, msc, ust)` → callback), mirroring `dispatch_event_surfaces_vblank_event`. Include a wrong-`length` / wrong-`type_` case → ignored.
- **Negative/garbage `time_ns`** → event dropped + logged, not converted to a bogus `Duration`.
- **Stale `crtc_id` on completion** → resolves to "no live output", event dropped, armed entry cleared (no panic, no fabricated output).
- **CRTC-scoped drain:** a flip on CRTC A with `msc≥T` does NOT fire a `PendingCompleteNotify` bound to CRTC B (the cross-clock satisfaction bug). A flip on the bound CRTC does.
- `pick_crtc(window)` max-coverage selection (single-output → that output; dual-output → larger intersection; off-screen → primary).
- **Arming dedup / no refire storm:** an already-passed absolute `(C,T)` is armed once, not re-issued every `drain_present_completions` iteration while in flight; arming CRTC A does not suppress arming CRTC B; the entry clears on completion and on `!scanout_allowed()`.
- **Side-effect-free `CRTC_SEQUENCE` handler:** assertion/fake that processing a sequence event mutates only MSC/UST + armed-map state, never scanout BO / scene state.

### Integration
- Existing 518-test suite stays green; `present_get_ust_msc` per-window tests updated for the CRTC binding.

### Manual hardware (REQUIRED before commit — see Risks)
1. **Cinnamon keyring (the bug):** on silence, `just yserver-cinnamon-hw` (direct, no x11trace). Trigger the keyring/polkit modal. PASS = panel clock keeps ticking while the modal is up AND the modal dismisses on the first click / accepts typing immediately. Confirm with `PRESENT-DBG`: idle `CRTC_SEQUENCE` events arrive at ~refresh rate (Δmsc≈1 per ~16ms), not +58/sec.
2. **No black-scanout regression** (the shelved `feature/deferred-present-completion` failure mode): MATE and a normal compositing session (xfce/picom) must render — not cursor-on-black. Test on **bee/RADV** specifically (where that regression was originally seen) plus silence.
3. **Idle CPU:** with strategy 1 (per-target absolute), an idle desktop must not spin arming vblanks every iteration; confirm idle CPU is comparable to pre-fix. (If strategy 2 is chosen, document the per-vblank wakeup cost.)
4. **VT switch / DPMS:** arming must be a no-op when `!scanout_allowed()`; switch away/back and DPMS off/on must not leak armed sequences or wedge.
5. Matrix smoke: bee, silence, yoga (rate canary).

## Risks

- **Black-scanout regression.** present-vblank-msc was never HW-validated; its sibling `feature/deferred-present-completion` was shelved for an empty-scanout BO → black screen on bee/RADV. This fix makes the deferred path *fire continuously*, exercising it harder. Mitigations (all in this spec): the `CRTC_SEQUENCE` handler is side-effect-free except MSC/UST + arm-clear (Part B invariant + unit assertion); arm only when `scanout_allowed()`; HW test #2 (bee/RADV) is a hard gate; keep `PRESENT-DBG` until verified.
- **Stale-routing race (was `user_data`=output_idx).** Resolved: `user_data` carries the stable `crtc_id`; `output_idx` is resolved at completion and the event dropped if no live output owns that CRTC (Part B). A delayed event across hotplug/VT-switch can no longer hit the wrong output.
- **Cross-clock completion satisfaction.** Resolved by the CRTC-scoped drain (Part C): a flip on CRTC A cannot fire a waiter bound to CRTC B even if MSC numbers happen to satisfy `target`.
- **Refire storm.** An already-passed absolute target armed every loop would spin; resolved by `(crtc, target)` dedup + the armed-target map (Part A). Covered by a unit test.
- **`time_ns` signedness.** `i64` → `u64::try_from`, drop+log negatives; never `as u64` (Part B). Sequence `u64` vs 32-bit hardware counter: reuse the backend's existing per-CRTC wrap bookkeeping, no second widening.
- **Raw ioctl correctness.** `_IOWR('d', 0x3C, drm_crtc_queue_sequence)` (24 bytes, read+write) — codex confirmed it matches the installed headers; still assert `size_of` at the call site. Sharing the DRM fd with `receive_events()` is safe as long as it stays serialized through the main loop (it is). `EOPNOTSUPP` on old kernels → relative-keep-alive fallback, logged once.
- **Stuck armed-target map → permanent stall.** The single-bool failure mode (stuck `true` after an aborted arm) is the whole bug class to avoid; the per-CRTC map MUST be cleared on `!scanout_allowed()`, output removal, and any advance event (see "Lifecycle & reconciliation"). Covered by unit tests.

## Rollout

1. Implement Parts A–C on `fix/present-vblank-msc-rebase` (on top of `698e112`).
2. `cargo +nightly fmt` / `cargo clippy` (regular) / `cargo test` green.
3. HW smoke (all five checks above) — user-observed, per the "no commit before HW smoke" rule.
4. Strip/demote `PRESENT-DBG` instrumentation.
5. Squash `698e112` + the fix into a coherent "per-CRTC vblank-paced Present completion (with atomic idle-vblank pacing)" change and merge to master.

## Follow-ups

- Dual-monitor non-monotonic MSC: confirm the per-window CRTC binding fully resolves it; if a window genuinely spans two CRTCs with skewed counters, decide a policy (report the bound CRTC's MSC only).
- Fold `present_get_ust_msc`'s remaining single-output assumptions once the CRTC binding is the single source of truth.
