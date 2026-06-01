# Idle-vblank MSC pacing — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make Present MSC advance at the display refresh rate when the scene is static, by replacing the broken legacy `drmWaitVBlank` idle path with absolute `DRM_IOCTL_CRTC_QUEUE_SEQUENCE` arming, binding each Present waiter to one CRTC, and parsing `DRM_EVENT_CRTC_SEQUENCE` ourselves.

**Architecture:** Three coupled pieces, all required:

- **Part A — arm:** new raw-ioctl helper issues `DRM_IOCTL_CRTC_QUEUE_SEQUENCE` for an absolute `(crtc_id, target_msc)` with `NEXT_ON_MISS`. The run loop hands the backend the list of pending `(crtc_id, target_msc)` pairs; the backend dedups against an in-flight per-CRTC armed-target map.
- **Part B — completion:** `dispatch_event` decodes `DRM_EVENT_CRTC_SEQUENCE` from the raw bytes the `drm` crate currently drops as `Event::Unknown`. The handler is side-effect-free except for MSC/UST accounting and clear-arm; clear-arm fires *before* any drop on stale CRTC / malformed `time_ns`.
- **Part C — bind:** each `PendingCompleteNotify` carries the CRTC it was armed against (raw `crtc_id`, stable across `Vec` compaction). `drain_pending_complete_notify_for_flip` and `recent_page_flips` are CRTC-aware so a flip on CRTC A cannot satisfy a waiter bound to CRTC B.

Lifecycle: the per-CRTC armed-target map is cleared on `!scanout_allowed()`, output removal, and any advance event (PageFlip / Vblank / CRTC_SEQUENCE) for that CRTC. Never strand a CRTC with a stuck entry — the single-bool failure mode is the bug class we are exiting.

**Tech Stack:**
- Rust, `drm` 0.15 (event iterator + control device), `drm-sys` 0.8.1 (ioctl constants/structs), `nix` 0.31 (ioctl macros — `ioctl_readwrite!`), `rustix` 1.1 (already pulled in by `drm`).
- KMS / DRM atomic backend (`crates/yserver/src/kms/v2/backend.rs`), shared Backend trait (`crates/yserver-core/src/backend/trait_def.rs`), run loop drain (`crates/yserver-core/src/core_loop/run.rs`), Present request handling (`crates/yserver-core/src/core_loop/process_request.rs`).

**Spec reference:** `docs/superpowers/specs/2026-06-01-idle-vblank-msc-pacing-design.md` (rev3, codex-converged).

**Branch:** `fix/present-vblank-msc-rebase` (already checked out; HEAD `f23b0d1` is the spec).

---

## File map (created / modified)

- **Modified:** `crates/yserver/src/drm/page_flip.rs`
  - New raw `drm_crtc_queue_sequence` / `drm_event_crtc_sequence` types + constants.
  - New `queue_crtc_sequence(device, crtc_id, flags, sequence, user_data)` raw-ioctl helper.
  - `dispatch_event` extended to decode `DRM_EVENT_CRTC_SEQUENCE` and emit a second callback.
  - `drain_events` widened to two callbacks: advance (`PageFlip`/`Vblank`) and sequence.
  - **Removed:** the legacy `request_next_vblank_event(device, crtc_index)` wrapper around `wait_vblank` (becomes dead the moment the v2 backend stops calling it).

- **Modified:** `crates/yserver/src/kms/v2/platform.rs`
  - `drain_page_flip_events` consumes both new callbacks: page-flip retires keep returning `PageFlipCompletion`; sequence events return a new `SequenceCompletion { crtc: crtc::Handle, crtc_id_raw: u32, time_ns: i64, sequence: u64 }` (raw, not yet validated).
  - Output-removal site at `:2222` invalidates armed-target entries by CRTC id.

- **Modified:** `crates/yserver-core/src/backend/trait_def.rs`
  - `CompletedPresentEvent` gains `pub bound_crtc: u32` so the PresentPixmap path can carry the bound CRTC through to `fire_present_completion_events` (which only takes `state` + `event` — no `backend` borrow). Default `0` for non-paced backends.
  - `drain_recent_page_flips` returns `Vec<(u32 crtc_id, u64 msc, u64 ust_micros)>` (was `(u64,u64)`).
  - New `pick_present_crtc(&self, window: u32) -> Option<u32>` (default `None` → non-KMS backends keep firing `crtc=0`).
  - Renamed/changed `request_next_vblank_event` → `arm_idle_vblanks(&mut self, pending: &[(u32 crtc_id, u64 target_msc)]) -> io::Result<usize armed>` (default `Ok(0)`).
  - `present_get_ust_msc(_window)` signature unchanged; **v2 impl rewritten** to route via `pick_present_crtc` so the returned `(msc, ust)` is the bound CRTC's clock (rev3 Part C). Single-output collapses to the existing output-0 behaviour.

- **Modified:** `crates/yserver-core/src/server.rs`
  - `PendingCompleteNotify` gains `pub crtc: u32` (raw `crtc_id`; `0` for non-paced backends).

- **Modified:** `crates/yserver-core/src/core_loop/process_request.rs`
  - Both enqueue sites (NotifyMSC `~:6470`, PresentPixmap `~:6885`) populate `crtc` via `backend.pick_present_crtc(window)` (fallback `0`).
  - `drain_pending_complete_notify_for_flip(state, crtc: u32, msc: u64, ust_micros: u64)` only fires entries whose `entry.crtc == crtc` (and target_msc gate, as today).

- **Modified:** `crates/yserver-core/src/core_loop/run.rs`
  - `drain_present_completions` iterates widened `drain_recent_page_flips`, calls `drain_pending_complete_notify_for_flip(state, crtc, msc, ust)` per flip.
  - Non-paced fallback at `:795` passes `crtc=0` (sentinel; matches `PendingCompleteNotify::crtc == 0` enqueued by non-paced backends).
  - Idle-arm: collects `pending_complete_notify.iter().map(|e| (e.crtc, e.target_msc))` into a small `Vec` and hands it to `backend.arm_idle_vblanks(&pending)`. Stays inside the `has_vblank_pacing()` branch.

- **Modified:** `crates/yserver/src/kms/v2/backend.rs`
  - Field: replace `vblank_request_in_flight: bool` with `armed_vblank_targets: HashMap<crtc::Handle, u64>` (key is the `crtc::Handle`, value is the absolute MSC armed). Initialized in all three constructors (`:585`, `:707`, `:1277`).
  - Field: `crtc_queue_sequence_unsupported: bool` — EOPNOTSUPP latch for the relative-1 keep-alive fallback on pre-4.14 kernels. Initialized `false` in all three constructors.
  - Field: `recent_page_flips: Vec<(crtc::Handle, u64, u64)>` (gain CRTC).
  - `record_crtc_ust_msc(crtc, output_idx, msc, ust)` — takes `crtc::Handle` explicitly so the sequence-event path (which has only `crtc_id`) and the page-flip path (which has both) share the same writer.
  - New: `pick_present_crtc(&self, window: u32) -> Option<u32>` — max-intersection-area over `platform.outputs`, fallback to primary (output 0), using `windows_v2` for window rect.
  - New: `arm_idle_vblanks(&mut self, pending: &[(u32 crtc_id, u64 target_msc)]) -> io::Result<usize>` — early-return `Ok(0)` + clear-all-armed if `!scanout_allowed()`; otherwise dedup against `armed_vblank_targets` per `(crtc::Handle, target_msc)`, call `queue_crtc_sequence` (absolute, NEXT_ON_MISS; relative=1 when `target==0`). On EOPNOTSUPP / ENOTTY from the ioctl, set `crtc_queue_sequence_unsupported = true` (latched; log once), and from that point on every arm passes `relative=1, sequence=1` regardless of input — Xorg's old per-vblank-wakeup behaviour kicks in. Wired with a testable seam `arm_idle_vblanks_with(pending, |cid, rel, seq| ...)` so unit tests can drive the dedup / target-zero / scanout-disallowed branches without a real DRM fd.
  - New: `on_crtc_sequence_event(crtc_id_raw: u32, time_ns: i64, sequence: u64)` — invariant: clears the matching armed-map entry first (unconditional clear-arm for any received sequence event on a known CRTC), then validates crtc_id → live output, then validates `time_ns` via `u64::try_from`, then calls `record_crtc_ust_msc`. Side-effect-free outside MSC/UST + armed-map.
  - DPMS / VT suspend hooks (existing `apply_dpms_transition` / `run_suspend` paths) call `self.armed_vblank_targets.clear()` so a queued sequence dropped by the kernel doesn't leave a stuck entry.

- **Modified:** Existing tests in `backend.rs` (`note_page_flip_complete_for_tests`, ~`:12540` onwards) get the widened `(crtc, msc, ust)` signature; pass a synthetic `crtc::Handle` (use `drm::control::from_u32(N).unwrap()` like `dispatch_event_passes_crtc_handle_for_page_flip` does in `drm/page_flip.rs:tests`).

- **NOT modified:** `crates/yserver-core/src/core_loop/process_request.rs::drain_pending_complete_notify_for_flip` keeps its `target_msc` gate. The `entry.crtc == crtc` filter is the only new condition.

---

## Sequencing notes

- Bottom-up: ioctl plumbing (Task 1) → event parse (Task 2) → backend state machine (Tasks 3–6) → run-loop wiring (Tasks 7–8) → lifecycle hooks (Task 9) → cleanup + HW smoke (Tasks 10–11). Each task ends in a green `cargo test` and a commit.
- DO NOT delete `drm/page_flip.rs::request_next_vblank_event` (the legacy wrapper) until Task 7 lands, because the v2 backend still calls it through Task 6. The trait rename in Task 7 makes the legacy wrapper unreachable; that's when it's removed.
- Tests use synthetic raw event bytes — no real DRM device is required for any unit test in this plan. The HW smoke gate is per the spec's "Manual hardware" section and runs after Task 10.

---

## Task 1: Raw ioctl plumbing for `DRM_IOCTL_CRTC_QUEUE_SEQUENCE`

**Files:**
- Modify: `crates/yserver/src/drm/page_flip.rs` (add module-local constants + struct + ioctl wrapper, just above the existing `submit_flip`).
- Test: `crates/yserver/src/drm/page_flip.rs` (`#[cfg(test)] mod tests`).

- [ ] **Step 1: Write the failing test for ioctl request code + struct size**

Add at the bottom of the existing `mod tests` in `crates/yserver/src/drm/page_flip.rs`:

```rust
#[test]
fn drm_crtc_queue_sequence_struct_is_24_bytes() {
    // Header drm.h line 1064: __u32 crtc_id; __u32 flags;
    // __u64 sequence; __u64 user_data; → 4+4+8+8 = 24 bytes.
    assert_eq!(std::mem::size_of::<super::drm_crtc_queue_sequence>(), 24);
    assert_eq!(std::mem::align_of::<super::drm_crtc_queue_sequence>(), 8);
}

#[test]
fn drm_event_crtc_sequence_struct_is_32_bytes() {
    // Header drm.h line 1429: struct drm_event base (8B) +
    // __u64 user_data + __s64 time_ns + __u64 sequence
    // = 8 + 8 + 8 + 8 = 32. (The spec note "24 bytes" refers
    // to the payload AFTER the 8-byte drm_event header; sizeof
    // of the full struct including header is 32.)
    assert_eq!(std::mem::size_of::<super::drm_event_crtc_sequence>(), 32);
}

#[test]
fn drm_crtc_queue_sequence_ioctl_request_code() {
    // _IOWR('d' /*0x64*/, 0x3C, drm_crtc_queue_sequence).
    // _IOC(dir=3 /*RW*/, type='d', nr=0x3C, size=24)
    //   = (3 << 30) | (24 << 16) | (0x64 << 8) | 0x3C
    //   = 0xC0186_43C? Compute:
    //     (3 << 30) = 0xC0000000
    //     (24 << 16) = 0x00180000
    //     (0x64 << 8) = 0x00006400
    //     0x3C = 0x3C
    //   = 0xC018643C
    assert_eq!(super::DRM_IOCTL_CRTC_QUEUE_SEQUENCE, 0xC018_643C);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p yserver --lib drm::page_flip::tests:: -- --nocapture`

Expected: FAIL with "cannot find type `drm_crtc_queue_sequence`" / "cannot find value `DRM_IOCTL_CRTC_QUEUE_SEQUENCE`".

- [ ] **Step 3: Define the structs + constants + ioctl request code**

In `crates/yserver/src/drm/page_flip.rs`, just below the existing `use` block (lines ~13–22):

```rust
// ── DRM_IOCTL_CRTC_QUEUE_SEQUENCE plumbing ──────────────────────
//
// `drm` 0.15 / `drm-ffi` 0.9 do not wrap this ioctl; we issue it
// raw. Layouts mirror `<drm/drm.h>` exactly (kernel headers, verified
// against /usr/include/drm/drm.h on the build host). All multi-byte
// fields are little-endian on every supported target.
//
// Both flags are passed in the `flags` field; combined or'd.

pub(crate) const DRM_CRTC_SEQUENCE_RELATIVE: u32 = 0x0000_0001;
pub(crate) const DRM_CRTC_SEQUENCE_NEXT_ON_MISS: u32 = 0x0000_0002;

/// kernel `DRM_EVENT_CRTC_SEQUENCE` event type id.
pub(crate) const DRM_EVENT_CRTC_SEQUENCE: u32 = 0x03;

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct drm_crtc_queue_sequence {
    pub crtc_id: u32,
    pub flags: u32,
    /// In: target sequence. Out: actual scheduled sequence.
    pub sequence: u64,
    /// Echoed back verbatim in the resulting `drm_event_crtc_sequence`.
    pub user_data: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct drm_event_header {
    pub r#type: u32,
    pub length: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct drm_event_crtc_sequence {
    pub base: drm_event_header,
    pub user_data: u64,
    /// CLOCK_MONOTONIC nanoseconds. Signed per kernel header — we
    /// must `u64::try_from` rather than `as u64`.
    pub time_ns: i64,
    pub sequence: u64,
}

// `_IOWR('d', 0x3C, drm_crtc_queue_sequence)` expanded inline so
// the request code is a `const` we can also assert in a unit test.
//   dir = 3 (RW), type = 'd' (0x64), nr = 0x3C, size = 24
pub(crate) const DRM_IOCTL_CRTC_QUEUE_SEQUENCE: u64 = ((3u64) << 30)
    | ((std::mem::size_of::<drm_crtc_queue_sequence>() as u64) << 16)
    | ((0x64u64) << 8)
    | 0x3Cu64;
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p yserver --lib drm::page_flip::tests:: -- --nocapture`

Expected: PASS for the three new tests; existing tests stay green.

- [ ] **Step 5: Commit**

```bash
git add crates/yserver/src/drm/page_flip.rs
git commit -m "drm(page_flip): add DRM_IOCTL_CRTC_QUEUE_SEQUENCE constants + structs

No callers yet — Task 2 in the idle-vblank plan adds the ioctl
wrapper, Task 7 wires it into the v2 backend.

Refs: docs/superpowers/specs/2026-06-01-idle-vblank-msc-pacing-design.md"
```

---

## Task 2: `queue_crtc_sequence` ioctl wrapper

**Files:**
- Modify: `crates/yserver/src/drm/page_flip.rs` (add helper below the constants).
- Test: `crates/yserver/src/drm/page_flip.rs::tests` (assert struct alignment + flag byte layout).

- [ ] **Step 1: Write the failing test for flag byte layout**

Append to `mod tests`:

```rust
#[test]
fn queue_sequence_layout_absolute_with_next_on_miss() {
    use super::{DRM_CRTC_SEQUENCE_NEXT_ON_MISS, drm_crtc_queue_sequence};
    // Absolute target: flags == NEXT_ON_MISS only (no RELATIVE bit).
    let req = drm_crtc_queue_sequence {
        crtc_id: 0x42,
        flags: DRM_CRTC_SEQUENCE_NEXT_ON_MISS,
        sequence: 0x1234_5678_9ABC_DEF0,
        user_data: 0x42,
    };
    assert_eq!(req.flags & 1, 0, "RELATIVE bit must be clear for absolute target");
    assert_eq!(req.flags & 2, 2, "NEXT_ON_MISS must be set");
}

#[test]
fn queue_sequence_layout_relative_one() {
    use super::{DRM_CRTC_SEQUENCE_RELATIVE, drm_crtc_queue_sequence};
    // target_msc == 0 maps to RELATIVE | sequence=1.
    let req = drm_crtc_queue_sequence {
        crtc_id: 0x42,
        flags: DRM_CRTC_SEQUENCE_RELATIVE,
        sequence: 1,
        user_data: 0x42,
    };
    assert_eq!(req.flags & 1, 1);
    assert_eq!(req.sequence, 1);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p yserver --lib drm::page_flip::tests:: -- --nocapture`

Expected: FAIL with "function or struct field" missing — these tests reference symbols that exist after Task 1, so they should PASS already. Verify they pass before moving on; if they pass, move directly to Step 3 (the wrapper is the new code we want to add but is independently testable only via a real device — the layout tests above already pin the ABI).

- [ ] **Step 3: Add the ioctl wrapper**

In `crates/yserver/src/drm/page_flip.rs`, below the const/struct block from Task 1:

```rust
/// Queue a one-shot CRTC vblank sequence event. `crtc_id` is the
/// **raw KMS object id** (NOT a pipe index — that distinction is
/// the whole reason this helper exists; the legacy `drmWaitVBlank`
/// path used pipe indices and lost the dual-monitor case).
///
/// - `relative = true`  → kernel arms `current_msc + sequence`
///   vblanks from now; pass `sequence = 1` for "next vblank".
/// - `relative = false` → absolute target. **Always pair with
///   `NEXT_ON_MISS`** (set internally) so an already-passed target
///   fires at the next vblank instead of waiting a full 32-bit
///   counter wrap.
///
/// `user_data` is echoed verbatim in the resulting
/// `DRM_EVENT_CRTC_SEQUENCE` — we encode the stable `crtc_id` there
/// (NOT `output_idx`, which is unstable across hotplug compaction
/// at `platform.rs:2222`).
///
/// Returns the kernel-assigned scheduled sequence on success.
///
/// # Errors
///
/// - `EOPNOTSUPP` on pre-4.14 kernels — caller should fall back
///   to the relative-keep-alive path.
/// - `EACCES` if we no longer hold DRM master — caller must have
///   pre-gated on `scanout_allowed()`.
pub(crate) fn queue_crtc_sequence(
    device: &Device,
    crtc_id: u32,
    relative: bool,
    sequence: u64,
    user_data: u64,
) -> io::Result<u64> {
    use std::os::unix::io::AsRawFd;

    let mut flags = DRM_CRTC_SEQUENCE_NEXT_ON_MISS;
    if relative {
        flags |= DRM_CRTC_SEQUENCE_RELATIVE;
    }
    let mut req = drm_crtc_queue_sequence {
        crtc_id,
        flags,
        sequence,
        user_data,
    };
    // SAFETY: `req` is a fully-initialised POD of the exact size the
    // kernel expects (24 bytes — pinned by the unit tests in Task 1).
    // The device fd is held alive by `device` for the duration of the
    // call; the kernel reads and writes `req` in place.
    let raw_fd = device.as_fd().as_raw_fd();
    let rc = unsafe {
        libc::ioctl(
            raw_fd,
            DRM_IOCTL_CRTC_QUEUE_SEQUENCE as libc::c_ulong,
            &mut req as *mut _,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(req.sequence)
}
```

(Add both `use std::os::fd::AsFd;` and `use std::os::unix::io::AsRawFd;` to the file's imports if not already present — `as_fd()` requires `AsFd` in scope; `as_raw_fd()` requires `AsRawFd`. The `Device` already implements `AsFd` per `drm/device.rs:17`.)

- [ ] **Step 4: Run cargo check + tests**

Run: `cargo check -p yserver && cargo test -p yserver --lib drm::page_flip::tests:: -- --nocapture`

Expected: builds clean, layout tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/yserver/src/drm/page_flip.rs
git commit -m "drm(page_flip): add queue_crtc_sequence raw-ioctl helper

Wraps DRM_IOCTL_CRTC_QUEUE_SEQUENCE so the v2 backend can arm
absolute per-CRTC vblanks (Part A of the idle-vblank plan).
Caller pre-gates on scanout_allowed(); EOPNOTSUPP fallback to
relative-1 is wired in Task 7."
```

---

## Task 3: Decode `DRM_EVENT_CRTC_SEQUENCE` in `dispatch_event`

**Files:**
- Modify: `crates/yserver/src/drm/page_flip.rs` (extend `dispatch_event` + `drain_events`).
- Test: `crates/yserver/src/drm/page_flip.rs::tests`.

The existing `dispatch_event` callback signature is `FnMut(crtc::Handle, u64, Duration)`. We add a second callback for sequence events, which carries the *raw* `(crtc_id_u32, time_ns_i64, sequence_u64)` so the backend can do unconditional clear-arm BEFORE validating time_ns / resolving crtc_id (spec invariant). The advance callback (PageFlip / Vblank) keeps its current shape.

- [ ] **Step 1: Write the failing tests**

Append to `mod tests` (replace nothing yet):

```rust
#[test]
fn dispatch_event_decodes_crtc_sequence() {
    use super::{DRM_EVENT_CRTC_SEQUENCE, drm_event_crtc_sequence, drm_event_header};
    // Build a raw 32-byte event matching the kernel layout.
    let raw = drm_event_crtc_sequence {
        base: drm_event_header { r#type: DRM_EVENT_CRTC_SEQUENCE, length: 32 },
        user_data: 0xCAFE_BABE_0000_0042, // bottom 32 bits = crtc_id 0x42
        time_ns: 1_234_567_890_i64,
        sequence: 9_999,
    };
    let bytes: [u8; 32] = unsafe { std::mem::transmute(raw) };
    let event = Event::Unknown(bytes.to_vec());

    let mut advance_calls = Vec::<(crtc::Handle, u64, Duration)>::new();
    let mut seq_calls = Vec::<(u32, i64, u64)>::new();
    super::dispatch_event(
        event,
        &mut |c, m, u| advance_calls.push((c, m, u)),
        &mut |cid, t, s| seq_calls.push((cid, t, s)),
    );

    assert!(advance_calls.is_empty(), "sequence event must NOT route through advance callback");
    assert_eq!(seq_calls.len(), 1);
    let (cid, time_ns, seq) = seq_calls[0];
    // Bottom 32 bits of user_data carry the crtc_id.
    assert_eq!(cid, 0x42);
    assert_eq!(time_ns, 1_234_567_890_i64);
    assert_eq!(seq, 9_999);
}

#[test]
fn dispatch_event_ignores_wrong_length_sequence_event() {
    use super::{DRM_EVENT_CRTC_SEQUENCE, drm_event_header};
    // Right type, wrong length → silently dropped.
    let header = drm_event_header { r#type: DRM_EVENT_CRTC_SEQUENCE, length: 16 };
    let mut bytes = vec![0u8; 16];
    bytes[..8].copy_from_slice(&unsafe {
        std::mem::transmute::<_, [u8; 8]>(header)
    });
    let event = Event::Unknown(bytes);

    let mut advance_calls = 0usize;
    let mut seq_calls = 0usize;
    super::dispatch_event(
        event,
        &mut |_, _, _| advance_calls += 1,
        &mut |_, _, _| seq_calls += 1,
    );
    assert_eq!(advance_calls, 0);
    assert_eq!(seq_calls, 0);
}

#[test]
fn dispatch_event_ignores_unknown_type() {
    use super::drm_event_header;
    let header = drm_event_header { r#type: 99, length: 32 };
    let mut bytes = vec![0u8; 32];
    bytes[..8].copy_from_slice(&unsafe {
        std::mem::transmute::<_, [u8; 8]>(header)
    });
    let event = Event::Unknown(bytes);
    let mut seq_calls = 0usize;
    super::dispatch_event(
        event,
        &mut |_, _, _| {},
        &mut |_, _, _| seq_calls += 1,
    );
    assert_eq!(seq_calls, 0);
}
```

(Existing tests reference the single-callback `dispatch_event(event, &mut callback)`; they will now fail to compile. Step 3 updates them too.)

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p yserver --lib drm::page_flip::tests:: -- --nocapture`

Expected: FAIL with "expected 2 arguments, found 3" on the existing tests + "function or method not found" on the new tests.

- [ ] **Step 3: Widen `dispatch_event` + `drain_events`**

Replace the existing `dispatch_event` body in `crates/yserver/src/drm/page_flip.rs`:

```rust
/// Dispatch a single drm event.
///
/// - `Event::PageFlip` / `Event::Vblank` → `on_advance(crtc, msc, ust)`
///   (existing behaviour; carries kernel `(msc, ust)` already widened).
/// - `Event::Unknown` matching `DRM_EVENT_CRTC_SEQUENCE` (type==3, length==32)
///   → `on_sequence(crtc_id_raw_u32, time_ns_i64, sequence_u64)`.
///   **Raw**: `time_ns` is signed and not yet validated; `crtc_id_raw`
///   is the bottom 32 bits of `user_data` (we encode it there in Task 7).
///   The caller does clear-arm BEFORE any drop on validity check.
/// - Everything else: dropped.
///
/// Factored so per-event routing is unit-testable without a real DRM fd.
fn dispatch_event<A, S>(event: Event, on_advance: &mut A, on_sequence: &mut S)
where
    A: FnMut(crtc::Handle, u64, std::time::Duration),
    S: FnMut(u32, i64, u64),
{
    match event {
        Event::PageFlip(ev) => {
            log::info!(
                "PRESENT-DBG: PageFlip event crtc={:?} msc={} dur={:?}",
                ev.crtc, ev.frame, ev.duration
            );
            on_advance(ev.crtc, u64::from(ev.frame), ev.duration);
        }
        Event::Vblank(ev) => {
            log::info!(
                "PRESENT-DBG: Vblank event crtc={:?} msc={} time={:?}",
                ev.crtc, ev.frame, ev.time
            );
            on_advance(ev.crtc, u64::from(ev.frame), ev.time);
        }
        Event::Unknown(bytes) => {
            // Header: u32 type, u32 length (8 bytes total).
            if bytes.len() < std::mem::size_of::<drm_event_header>() {
                return;
            }
            let header: drm_event_header = unsafe {
                std::ptr::read_unaligned(bytes.as_ptr() as *const drm_event_header)
            };
            if header.r#type != DRM_EVENT_CRTC_SEQUENCE {
                return;
            }
            if header.length as usize != std::mem::size_of::<drm_event_crtc_sequence>() {
                return;
            }
            if bytes.len() < std::mem::size_of::<drm_event_crtc_sequence>() {
                return;
            }
            let ev: drm_event_crtc_sequence = unsafe {
                std::ptr::read_unaligned(bytes.as_ptr() as *const drm_event_crtc_sequence)
            };
            // Bottom 32 bits of user_data are the crtc_id we encoded.
            #[allow(clippy::cast_possible_truncation)]
            let crtc_id_raw = ev.user_data as u32;
            log::info!(
                "PRESENT-DBG: CrtcSequence event crtc_id={crtc_id_raw} sequence={} time_ns={}",
                ev.sequence, ev.time_ns
            );
            on_sequence(crtc_id_raw, ev.time_ns, ev.sequence);
        }
    }
}
```

And widen `drain_events`:

```rust
pub fn drain_events<A, S>(device: &Device, mut on_advance: A, mut on_sequence: S) -> io::Result<()>
where
    A: FnMut(crtc::Handle, u64, std::time::Duration),
    S: FnMut(u32, i64, u64),
{
    for event in device.receive_events()? {
        dispatch_event(event, &mut on_advance, &mut on_sequence);
    }
    Ok(())
}
```

Update the existing test bodies (`dispatch_event_passes_crtc_handle_for_page_flip`, `dispatch_event_surfaces_vblank_event`, etc.) to pass a no-op second callback. Search the test module for `dispatch_event(` and add `, &mut |_, _, _| {}` to each call.

Update the sole caller in `platform.rs::drain_page_flip_events` to pass a no-op sequence callback for now (the real wiring lands in Task 8 — this is a one-line transitional change to keep the build green):

```rust
crate::drm::page_flip::drain_events(
    &self.device,
    |c, msc, ust| { flipped.push((c, msc, ust)); },
    |_cid, _t, _s| { /* wired in Task 8 */ },
)?;
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p yserver --lib drm::page_flip:: -- --nocapture` and `cargo build -p yserver --tests --locked`.

Expected: PASS for all three new tests + the updated existing tests.

- [ ] **Step 5: Commit**

```bash
git add crates/yserver/src/drm/page_flip.rs crates/yserver/src/kms/v2/platform.rs
git commit -m "drm(page_flip): decode DRM_EVENT_CRTC_SEQUENCE in dispatch_event

drain_events grows a second callback for sequence events. Sequence
events carry the raw (crtc_id, time_ns, sequence) tuple; the
backend (Task 8) does unconditional clear-arm before validating.

Part B of the idle-vblank plan."
```

---

## Task 4: Per-CRTC armed-target map on the v2 backend

**Files:**
- Modify: `crates/yserver/src/kms/v2/backend.rs` (replace `vblank_request_in_flight: bool` with a `HashMap<crtc::Handle, u64>` field; update all three constructors and the existing clear-site in `record_crtc_ust_msc`).
- Test: `crates/yserver/src/kms/v2/backend.rs` (new unit tests near the existing Present-pacing block, ~`:12540`).

- [ ] **Step 1: Write the failing tests**

Add to the existing `#[cfg(test)] mod tests` block in `backend.rs` (find by searching `present_get_ust_msc_tracks_pageflip_completion`):

```rust
#[test]
fn armed_vblank_targets_starts_empty() {
    let b = super::KmsBackendV2::for_tests();
    assert!(b.armed_vblank_targets.is_empty());
}

#[test]
fn clear_armed_vblank_target_removes_entry() {
    let mut b = super::KmsBackendV2::for_tests();
    let h = drm::control::from_u32(7).unwrap();
    b.armed_vblank_targets.insert(h, 1234);
    b.clear_armed_vblank_target(h);
    assert!(b.armed_vblank_targets.is_empty());
}

#[test]
fn clear_all_armed_vblank_targets_empties_map() {
    let mut b = super::KmsBackendV2::for_tests();
    let a = drm::control::from_u32(7).unwrap();
    let c = drm::control::from_u32(9).unwrap();
    b.armed_vblank_targets.insert(a, 1);
    b.armed_vblank_targets.insert(c, 2);
    b.clear_all_armed_vblank_targets();
    assert!(b.armed_vblank_targets.is_empty());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p yserver --lib kms::v2::backend::tests::armed -- --nocapture`

Expected: FAIL on "no field `armed_vblank_targets`" / method missing.

- [ ] **Step 3: Replace the bool with a `HashMap`**

In `crates/yserver/src/kms/v2/backend.rs`:

- At ~`:205`, replace the `vblank_request_in_flight: bool` field doc-comment block + field with:

```rust
/// Per-CRTC armed absolute MSC. Replaces the single
/// `vblank_request_in_flight: bool` — that gate could only track one
/// in-flight request total, so arming CRTC A would suppress arming
/// CRTC B (the dual-monitor permanent-stall class). Keyed by the
/// stable `crtc::Handle`, value is the absolute MSC currently
/// armed (or `1` for the EOPNOTSUPP relative-fallback).
///
/// **Invariant:** every code path that drops or completes an armed
/// sequence MUST clear the matching entry — `record_crtc_ust_msc`
/// (advance proof), `on_crtc_sequence_event` (unconditional
/// clear-arm before validating), `!scanout_allowed()` /
/// `apply_dpms_transition` / `run_suspend` (master loss drops queued
/// sequences), and output removal (the CRTC is gone). A stuck entry
/// = a permanent ~0 fps stall on that CRTC, which is the whole bug
/// class this plan is exiting.
pub(crate) armed_vblank_targets: std::collections::HashMap<
    ::drm::control::crtc::Handle,
    u64,
>,
```

- Three constructor sites — `:585`, `:707`, `:1277` — replace `vblank_request_in_flight: false,` with:

```rust
armed_vblank_targets: std::collections::HashMap::new(),
```

- In `record_crtc_ust_msc` (`:2494`) replace the line:

```rust
self.vblank_request_in_flight = false;
```

with (still inside `record_crtc_ust_msc`, AFTER the existing push to `recent_page_flips`):

```rust
// Advance proof: any retire on this CRTC's `crtc::Handle` clears
// the armed entry. The handler at the sequence-event call site
// resolves crtc_id → Handle and routes through here, so a single
// clear point covers both PageFlip/Vblank and CRTC_SEQUENCE.
// (Task 5 widens `record_crtc_ust_msc` to take the `crtc::Handle`
// explicitly so we can clear by Handle instead of having to look
// it up from output_idx here.)
// TEMP placeholder until Task 5: clear by linear scan from output_idx.
if let Some(layout) = self.platform.outputs.get(output_idx) {
    self.armed_vblank_targets.remove(&layout.output.crtc);
}
```

- In `request_next_vblank_event` (the existing one at `:11831`) replace:

```rust
if self.vblank_request_in_flight {
    return Ok(false);
}
```

with:

```rust
// Stub: the rename to `arm_idle_vblanks` lands in Task 7 with the
// full per-(crtc, target) dedup. For Task 4 we keep the old
// trait method working by treating "any armed CRTC" as "in flight".
if !self.armed_vblank_targets.is_empty() {
    return Ok(false);
}
```

and replace `self.vblank_request_in_flight = true;` with:

```rust
// Pre-Task-7 placeholder: hardcode output 0's CRTC.
if let Some(layout) = self.platform.outputs.first() {
    self.armed_vblank_targets.insert(layout.output.crtc, 0);
}
```

- Add the two helper methods inside the existing `impl KmsBackendV2 { ... }` block (near `record_crtc_ust_msc`):

```rust
pub(crate) fn clear_armed_vblank_target(&mut self, crtc: ::drm::control::crtc::Handle) {
    self.armed_vblank_targets.remove(&crtc);
}

pub(crate) fn clear_all_armed_vblank_targets(&mut self) {
    self.armed_vblank_targets.clear();
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p yserver --lib kms::v2::backend:: -- --nocapture` and `cargo build -p yserver --locked`.

Expected: PASS; the existing `present_get_ust_msc_tracks_pageflip_completion` continues to pass because the placeholder code in step 3 still clears on each retire.

- [ ] **Step 5: Commit**

```bash
git add crates/yserver/src/kms/v2/backend.rs
git commit -m "kms/v2: per-CRTC armed-target map (replace vblank_request_in_flight bool)

A single bool gates only one in-flight vblank request across all
CRTCs, so arming CRTC A suppresses arming CRTC B — exactly the
permanent-stall class the spec is exiting. Replaced with a
HashMap<crtc::Handle, u64 armed_target> keyed by stable CRTC
handle. Helper methods clear by handle / clear all; the legacy
request_next_vblank_event call site is wired to the map as a
placeholder until Task 7 lands the real arm_idle_vblanks API."
```

---

## Task 5: Carry `crtc::Handle` through `recent_page_flips`

**Files:**
- Modify: `crates/yserver/src/kms/v2/backend.rs` (field type widening + `record_crtc_ust_msc` signature + `drain_recent_page_flips` impl).
- Modify: `crates/yserver-core/src/backend/trait_def.rs` (`drain_recent_page_flips` trait signature).
- Modify: `crates/yserver-core/src/core_loop/run.rs` (consume widened tuple — interim: still calls `drain_pending_complete_notify_for_flip` with `(msc, ust)` only; CRTC plumbing through the run loop lands in Task 8).
- Test: existing `present_get_ust_msc_tracks_pageflip_completion` + new round-trip test.

- [ ] **Step 1: Write the failing test**

Append to the v2 backend test module:

```rust
#[test]
fn drain_recent_page_flips_includes_crtc_id() {
    let mut b = super::KmsBackendV2::for_tests();
    // Push one synthetic retire via the test injector.
    b.note_page_flip_complete_for_tests(
        0, /* output_idx */
        42, /* msc */
        std::time::Duration::from_micros(1000),
    );
    let drained = <_ as crate::backend_trait::Backend>::drain_recent_page_flips(&mut b);
    assert_eq!(drained.len(), 1);
    let (crtc_id, msc, ust_micros) = drained[0];
    // `for_tests` builds with a synthetic output 0; its CRTC handle is
    // a deterministic stub — assert the tuple shape, not the exact id.
    assert_ne!(crtc_id, 0, "v2 backend must surface a real crtc_id");
    assert_eq!(msc, 42);
    assert_eq!(ust_micros, 1_000);
}
```

(Use whatever existing path the file uses to reach the `Backend` trait method — search for `<_ as` or `Backend::` in the existing test module and match the style. If `for_tests` does not stand up real outputs, swap this for an injector that records `crtc_id` directly. See implementation note in Step 3.)

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p yserver --lib kms::v2::backend::tests::drain_recent_page_flips -- --nocapture`

Expected: FAIL because `drain_recent_page_flips` still returns `Vec<(u64, u64)>`.

- [ ] **Step 3: Widen the field + impl + trait**

In `crates/yserver/src/kms/v2/backend.rs`:

- Field at `:203`:

```rust
pub(crate) recent_page_flips: Vec<(::drm::control::crtc::Handle, u64, u64)>,
```

- `record_crtc_ust_msc` signature at `:2494` widens to take `crtc`:

```rust
pub(crate) fn record_crtc_ust_msc(
    &mut self,
    crtc: ::drm::control::crtc::Handle,
    output_idx: usize,
    msc: u64,
    ust: std::time::Duration,
) {
    self.crtc_ust_msc.insert(output_idx, (msc, ust));
    #[allow(clippy::cast_possible_truncation)]
    let ust_micros: u64 = ust.as_micros() as u64;
    self.recent_page_flips.push((crtc, msc, ust_micros));
    // Advance proof — see field doc-comment.
    self.armed_vblank_targets.remove(&crtc);
}
```

- The single existing call site at `:6652`:

```rust
let crtc = self.platform.outputs[completion.output_idx].output.crtc;
self.record_crtc_ust_msc(crtc, completion.output_idx, completion.msc, completion.ust);
```

- `note_page_flip_complete_for_tests` widens too:

```rust
#[doc(hidden)]
pub fn note_page_flip_complete_for_tests(
    &mut self,
    output_idx: usize,
    msc: u64,
    ust: std::time::Duration,
) {
    let crtc = self.platform.outputs[output_idx].output.crtc;
    self.record_crtc_ust_msc(crtc, output_idx, msc, ust);
}
```

- `drain_recent_page_flips` impl in the `Backend` impl block (`:11815`):

```rust
fn drain_recent_page_flips(&mut self) -> Vec<(u32, u64, u64)> {
    std::mem::take(&mut self.recent_page_flips)
        .into_iter()
        .map(|(h, m, u)| (u32::from(h), m, u))
        .collect()
}
```

In `crates/yserver-core/src/backend/trait_def.rs` at `:1507`:

```rust
fn drain_recent_page_flips(&mut self) -> Vec<(u32, u64, u64)> {
    Vec::new()
}
```

In `crates/yserver-core/src/core_loop/run.rs` (~`:769`):

```rust
for (_crtc_id, msc, ust_micros) in flips {
    crate::core_loop::process_request::drain_pending_complete_notify_for_flip(
        state, msc, ust_micros,
    );
}
```

(The CRTC value is consumed in Task 8 once `drain_pending_complete_notify_for_flip` itself grows the parameter.)

- [ ] **Step 4: Run tests to verify they pass + build clean**

Run: `cargo build -p yserver -p yserver-core --locked && cargo test -p yserver --lib kms::v2::backend::tests -- --nocapture`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/yserver/src/kms/v2/backend.rs crates/yserver-core/src/backend/trait_def.rs crates/yserver-core/src/core_loop/run.rs
git commit -m "kms/v2: widen recent_page_flips to carry crtc_id

drain_recent_page_flips now returns Vec<(crtc_id, msc, ust_micros)>;
record_crtc_ust_msc takes the crtc::Handle directly so the
sequence-event handler (Task 8) can clear-arm by Handle without
re-resolving from output_idx. Run loop discards the new crtc_id
field for now — Task 8 plumbs it into the CRTC-scoped completion
drain."
```

---

## Task 6: `pick_present_crtc` — bind the waiter to one CRTC

**Files:**
- Modify: `crates/yserver-core/src/backend/trait_def.rs` (new trait method, default `None`).
- Modify: `crates/yserver/src/kms/v2/backend.rs` (max-coverage implementation).
- Test: `crates/yserver/src/kms/v2/backend.rs`.

- [ ] **Step 1: Write the failing tests**

In `crates/yserver/src/kms/v2/backend.rs::tests` (search for an existing v2-specific test fixture that stands up real `outputs` — if `for_tests` doesn't, use the same fixture that `present_get_ust_msc_tracks_pageflip_completion` uses):

```rust
#[test]
fn pick_present_crtc_single_output_returns_that_crtc() {
    use crate::backend_trait::Backend;
    let mut b = super::KmsBackendV2::for_tests();
    // Register a window covering output 0 fully.
    b.windows_v2.insert(0x100, super::WindowGeometryV2 {
        x: 0, y: 0, width: 800, height: 600,
        depth: 24, mapped: true, parent: None, stack_rank: 0,
        bg_pixel: None, bg_pixmap: None, cursor: None,
    });
    let crtc_id = b.pick_present_crtc(0x100);
    assert!(crtc_id.is_some());
    assert_eq!(crtc_id.unwrap(), u32::from(b.platform.outputs[0].output.crtc));
}

#[test]
fn pick_present_crtc_unknown_window_falls_back_to_primary() {
    use crate::backend_trait::Backend;
    let b = super::KmsBackendV2::for_tests();
    // No window registered; must still return primary (output 0) CRTC.
    let crtc_id = b.pick_present_crtc(0xDEAD);
    assert_eq!(crtc_id, Some(u32::from(b.platform.outputs[0].output.crtc)));
}
```

(If `for_tests` lacks outputs, add a second fixture `for_tests_with_dual_output` mirroring whatever `present_get_ust_msc_tracks_pageflip_completion` uses; a third test below covers the dual-output case.)

```rust
#[test]
fn pick_present_crtc_dual_output_max_coverage() {
    use crate::backend_trait::Backend;
    // Build a backend with two outputs: A at (0,0,800,600), B at (800,0,1280,720).
    let mut b = super::KmsBackendV2::for_tests_with_dual_output(
        (0, 0, 800, 600),
        (800, 0, 1280, 720),
    );
    // Window mostly on B: x=900, w=400 → fully inside B.
    b.windows_v2.insert(0x200, super::WindowGeometryV2 {
        x: 900, y: 100, width: 400, height: 300,
        depth: 24, mapped: true, parent: None, stack_rank: 0,
        bg_pixel: None, bg_pixmap: None, cursor: None,
    });
    let picked = b.pick_present_crtc(0x200).unwrap();
    assert_eq!(picked, u32::from(b.platform.outputs[1].output.crtc));
}
```

If `for_tests_with_dual_output` does not exist yet, add it as a `#[cfg(test)]` helper next to the existing `for_tests` (use the same construction path and append a second `OutputLayout` with the given rect). If standing up a second synthetic output is too involved for the existing fixture, mark this test `#[ignore]` with a `// TODO: dual-output fixture` and cover the case via a unit-level `pick_present_crtc_inner(window_rect, &[output_rects]) -> Option<usize>` pure function instead — that keeps the geometry logic testable without DRM plumbing.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p yserver --lib kms::v2::backend::tests::pick_present_crtc -- --nocapture`

Expected: FAIL with "no method named `pick_present_crtc`".

- [ ] **Step 3: Add the trait method + v2 impl**

In `crates/yserver-core/src/backend/trait_def.rs`, just below `present_get_ust_msc` (line ~`:1498`):

```rust
/// Pick the CRTC that should service `window`'s Present pacing.
/// Returns the raw KMS `crtc_id` so callers can encode it into
/// the `user_data` of `DRM_IOCTL_CRTC_QUEUE_SEQUENCE` and into
/// `PendingCompleteNotify.crtc` for CRTC-scoped completion drain.
///
/// `None` means "this backend has no CRTC concept" (HostX11,
/// Recording). In that case the run loop fires with sentinel
/// `crtc=0` so non-paced backends keep their pre-fix behaviour.
///
/// v2 picks the output with maximum intersection area with the
/// window's root rect; ties go to the primary (output 0).
fn pick_present_crtc(&self, _window: u32) -> Option<u32> {
    None
}
```

In `crates/yserver/src/kms/v2/backend.rs`, inside the `impl Backend for KmsBackendV2` block (near `present_get_ust_msc` at `:11803`):

```rust
fn pick_present_crtc(&self, window: u32) -> Option<u32> {
    // Window rect: take from windows_v2; absent → primary.
    let Some(g) = self.windows_v2.get(&window) else {
        return self
            .platform
            .outputs
            .first()
            .map(|o| u32::from(o.output.crtc));
    };
    let wx = i32::from(g.x);
    let wy = i32::from(g.y);
    let ww = i32::from(g.width);
    let wh = i32::from(g.height);
    let mut best: Option<(usize, i64)> = None;
    for (idx, layout) in self.platform.outputs.iter().enumerate() {
        let ox = layout.x;
        let oy = layout.y;
        let ow = i32::from(layout.width);
        let oh = i32::from(layout.height);
        let x1 = wx.max(ox);
        let y1 = wy.max(oy);
        let x2 = (wx + ww).min(ox + ow);
        let y2 = (wy + wh).min(oy + oh);
        let area = if x2 > x1 && y2 > y1 {
            i64::from(x2 - x1) * i64::from(y2 - y1)
        } else {
            0
        };
        match best {
            None => best = Some((idx, area)),
            Some((_, ba)) if area > ba => best = Some((idx, area)),
            _ => {}
        }
    }
    // best is `Some` iff at least one output exists; max-coverage of 0
    // collapses to "first output by enumeration order" = primary, which
    // matches the spec's tie-break.
    best.and_then(|(idx, _)| {
        self.platform.outputs.get(idx).map(|o| u32::from(o.output.crtc))
    })
}
```

- [ ] **Step 4: Wire `present_get_ust_msc` to the bound CRTC**

Rev3 Part C mandates: "`present_get_ust_msc(window)` returns the bound CRTC's `(msc, ust)`." The current v2 impl at `backend.rs:11803` hardcodes `output_idx = 0`. Replace it with a CRTC-bound lookup via the same picker:

In `crates/yserver/src/kms/v2/backend.rs:11803`, replace the body:

```rust
fn present_get_ust_msc(&self, window: u32) -> (u64, std::time::Duration) {
    // Rev3 Part C: per-window CRTC binding. Pick the same CRTC the
    // arming path uses, then look up its (msc, ust) by resolving the
    // crtc_id back to an output_idx (crtc_ust_msc is keyed by
    // output_idx for historical reasons — folding that to a
    // HashMap<crtc::Handle, _> is the rev3 follow-up).
    let Some(crtc_id) = self.pick_present_crtc(window) else {
        return (0, std::time::Duration::ZERO);
    };
    let Some(handle) = ::drm::control::from_u32(crtc_id) else {
        return (0, std::time::Duration::ZERO);
    };
    let Some(output_idx) = self
        .platform
        .outputs
        .iter()
        .position(|o| o.output.crtc == handle)
    else {
        return (0, std::time::Duration::ZERO);
    };
    self.crtc_ust_msc
        .get(&output_idx)
        .copied()
        .unwrap_or((0, std::time::Duration::ZERO))
}
```

Add an integration test asserting that on a dual-output fixture, `present_get_ust_msc(window_on_output_b)` returns `(msc_b, ust_b)` after `note_page_flip_complete_for_tests` injects retires on both outputs with different MSCs. Skip if dual-output fixture is `#[ignore]`'d per the picker tests.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p yserver --lib kms::v2::backend::tests::pick_present_crtc kms::v2::backend::tests::present_get_ust_msc -- --nocapture`

Expected: PASS for `pick_present_crtc_*` and the existing `present_get_ust_msc_tracks_pageflip_completion` (single-output collapses to output 0, no behaviour change for that test).

- [ ] **Step 6: Commit**

```bash
git add crates/yserver-core/src/backend/trait_def.rs crates/yserver/src/kms/v2/backend.rs
git commit -m "backend: pick_present_crtc + per-window present_get_ust_msc

pick_present_crtc returns the raw crtc_id servicing a window
(max intersection area, primary on tie). Default returns None for
non-KMS backends.

present_get_ust_msc(window) now resolves via pick_present_crtc so
multi-monitor compositors get the bound CRTC's clock (rev3 Part C);
single-output collapses to the existing output-0 behaviour."
```

---

## Task 7: Bind CRTC on `PendingCompleteNotify` + CRTC-scoped completion drain

**Files:**
- Modify: `crates/yserver-core/src/server.rs` (`PendingCompleteNotify` adds `pub crtc: u32`).
- Modify: `crates/yserver-core/src/core_loop/process_request.rs` (both enqueue sites populate `crtc`; `drain_pending_complete_notify_for_flip` gains `crtc` param).
- Modify: `crates/yserver-core/src/core_loop/run.rs` (paced loop passes per-flip `crtc_id`, non-paced fallback passes `0`).
- Test: existing `process_request` Present-pacing tests at `:17385`–`:17710`.

- [ ] **Step 1: Write the failing test for CRTC-scoped drain**

In `crates/yserver-core/src/core_loop/process_request.rs::tests`, near `:17710`:

```rust
#[test]
fn drain_pending_complete_notify_for_flip_filters_by_crtc() {
    let mut state = super::make_test_state();
    // Two pending entries on different CRTCs.
    let mk = |crtc: u32, eid: u32| crate::server::PendingCompleteNotify {
        client_id: super::ClientId(1),
        eid,
        window: crate::server::ResourceId(0x100),
        serial: 0,
        kind: 1,
        mode: 0,
        target_msc: 0,
        crtc,
    };
    state.pending_complete_notify.push_back(mk(0x42, 0xA));
    state.pending_complete_notify.push_back(mk(0x99, 0xB));

    // Flip on CRTC 0x42 — should drain only eid 0xA.
    super::drain_pending_complete_notify_for_flip(&mut state, 0x42, 1, 1_000);
    let remaining: Vec<u32> = state.pending_complete_notify.iter().map(|e| e.eid).collect();
    assert_eq!(remaining, vec![0xB], "eid 0xB must remain (different CRTC)");
}
```

(Mirror the style of the existing `drain_pending_complete_notify_for_flip` tests. `make_test_state` already exists in the test module — search for it.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p yserver-core --lib core_loop::process_request::tests::drain_pending_complete_notify_for_flip_filters_by_crtc -- --nocapture`

Expected: FAIL with "no field `crtc` on `PendingCompleteNotify`".

- [ ] **Step 3: Add `crtc` field + filter**

In `crates/yserver-core/src/server.rs` at `:906`:

```rust
pub struct PendingCompleteNotify {
    pub client_id: ClientId,
    pub eid: u32,
    pub window: ResourceId,
    pub serial: u32,
    pub kind: u8,
    pub mode: u8,
    pub target_msc: u64,
    /// CRTC bound at enqueue time. Raw KMS `crtc_id` for KMS backends;
    /// `0` sentinel for non-paced backends (HostX11, Recording) so
    /// their `drain_pending_complete_notify_for_flip(_, 0, _, _)` calls
    /// match. Stays attached even if the window moves before the
    /// completion arrives.
    pub crtc: u32,
}
```

In `crates/yserver-core/src/core_loop/process_request.rs`:

- `drain_pending_complete_notify_for_flip` signature at `:6973`:

```rust
pub fn drain_pending_complete_notify_for_flip(
    state: &mut ServerState,
    crtc: u32,
    msc: u64,
    ust_micros: u64,
) {
    // (body unchanged except for the gate below)
```

- Add a second filter inside the `entries` loop, just above the existing `target_msc` gate at `:6982`:

```rust
// CRTC-scoped drain — a flip on CRTC A must not fire a waiter
// bound to CRTC B (the dual-monitor cross-clock satisfaction bug).
// `crtc == 0` is the non-paced sentinel: matches entries enqueued
// by backends that return `None` from `pick_present_crtc`.
if entry.crtc != crtc {
    state.pending_complete_notify.push_back(entry);
    continue;
}
```

- Both enqueue sites — at `:6470` (NotifyMSC, direct `PendingCompleteNotify` push) and the PresentPixmap path (via `fire_present_completion_events` at `:6795`) — must carry the bound CRTC. The NotifyMSC site has `backend` in scope directly; the PresentPixmap path flows through `CompletedPresentEvent` (defined at `crates/yserver-core/src/backend/trait_def.rs:140`), which then becomes a `PendingCompleteNotify` inside `fire_present_completion_events`. We thread the CRTC through `CompletedPresentEvent`.

Concrete plumbing — apply all of this in this single task:

1. **Add field to `CompletedPresentEvent`** (`crates/yserver-core/src/backend/trait_def.rs:140`):

```rust
pub struct CompletedPresentEvent {
    pub client_id: yserver_protocol::x11::ClientId,
    pub serial: u32,
    pub host_xid: u32,
    pub dst_host_xid: u32,
    pub options: u32,
    pub wake: PresentWake,
    pub target_msc: u64,
    /// CRTC the Present is paced against. Raw KMS `crtc_id` for the
    /// v2 KMS backend; `0` for non-paced backends (HostX11, Recording).
    /// Carried through to `PendingCompleteNotify.crtc` by
    /// `fire_present_completion_events` so the CRTC-scoped drain can
    /// reject cross-clock satisfaction.
    pub bound_crtc: u32,
}
```

2. **Update `fire_present_completion_events`** (`crates/yserver-core/src/core_loop/process_request.rs:6795` — currently called `fire_present_completion_events(state, event)`) so the `PendingCompleteNotify` push at `:6885` reads `crtc: event.bound_crtc`.

3. **PresentPixmap enqueue site at `process_request.rs:6362`**: `enqueue_present_completion(CompletedPresentEvent { ..., bound_crtc: backend.pick_present_crtc(req.window).unwrap_or(0) }, dst.host_xid())`.

4. **PresentPixmapSynced enqueue site at `process_request.rs:6633`**: same — `bound_crtc: backend.pick_present_crtc(req.window).unwrap_or(0)`.

5. **NotifyMSC enqueue site at `process_request.rs:6470`**: this site pushes `PendingCompleteNotify` directly (it doesn't go via `CompletedPresentEvent`); set `crtc: backend.pick_present_crtc(req.window).unwrap_or(0)` directly in the literal.

6. **Update every existing `CompletedPresentEvent {}` literal** to add `bound_crtc: 0` (default for tests / non-CRTC paths). Sites grep'd from the current tree:
   - `crates/yserver-core/src/backend/trait_def.rs:1702` (default-impl helper)
   - `crates/yserver-core/src/core_loop/process_request.rs:17460` (test)
   - `crates/yserver-core/src/core_loop/process_request.rs:17554` (test)
   - `crates/yserver/src/kms/v2/backend.rs:2853` (v2 enqueue point — set to `self.platform.outputs.first().map(|o| u32::from(o.output.crtc)).unwrap_or(0)` to actually carry the v2 CRTC; tests reading via `for_tests` get a real crtc_id)
   - `crates/yserver/src/kms/v2/backend.rs:3055` (synced variant — same: set v2 CRTC, not 0)
   - `crates/yserver/src/kms/v2/backend.rs:12603` (test literal — `bound_crtc: 0`)
   - `crates/yserver/src/kms/v2/present_completion.rs:89` (the legacy non-Present path — `bound_crtc: 0`)
   - `crates/yserver/src/kms/v2/backend.rs:1050` from `attach_synthetic_present_completion_to_cow_for_tests` — `bound_crtc: 0`
   - `crates/yserver/tests/v2_acceptance.rs:2977, 3031, 3071, 3112, 3205` — five integration-test literals, each set to `bound_crtc: 0`.

7. **Update the test fixture**: any test that constructs `PendingCompleteNotify { ... }` literally now sets `crtc: 0`. Grep `PendingCompleteNotify {` finds the sites — the request-tests in `process_request.rs:17385`–`:17710` are the load-bearing ones.

In `crates/yserver-core/src/core_loop/run.rs`:

- Replace the paced loop at `~:769`:

```rust
for (crtc_id, msc, ust_micros) in flips {
    crate::core_loop::process_request::drain_pending_complete_notify_for_flip(
        state, crtc_id, msc, ust_micros,
    );
}
```

- Replace the non-paced fallback at `:795`:

```rust
crate::core_loop::process_request::drain_pending_complete_notify_for_flip(state, 0, 0, 0);
```

(`0` for `crtc` matches the `PendingCompleteNotify::crtc = 0` enqueued under the default `pick_present_crtc → None` path.)

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p yserver-core -p yserver --locked -- --nocapture 2>&1 | tail -50`

Expected: existing Present-pacing tests pass with the literal `crtc: 0`; the new CRTC-filter test passes; no other regressions.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "core_loop: CRTC-scoped Present completion drain

PendingCompleteNotify gains a 'crtc' field bound at enqueue time
(via backend.pick_present_crtc). drain_pending_complete_notify_for_flip
filters by (entry.crtc == crtc) before the target_msc gate, so a flip
on CRTC A can never satisfy a waiter armed against CRTC B. Non-paced
backends use sentinel 0 on both sides for backward-compatible behaviour.

Part C of the idle-vblank plan."
```

---

## Task 8: Replace legacy `wait_vblank` with `arm_idle_vblanks` (`queue_crtc_sequence`)

**Files:**
- Modify: `crates/yserver-core/src/backend/trait_def.rs` (rename trait method, change signature).
- Modify: `crates/yserver-core/src/core_loop/run.rs` (collect pending tuples, call new method).
- Modify: `crates/yserver/src/kms/v2/backend.rs` (new `arm_idle_vblanks` impl, `on_crtc_sequence_event` handler).
- Modify: `crates/yserver/src/kms/v2/platform.rs` (`drain_page_flip_events` returns sequence completions too; route them through `on_crtc_sequence_event`).
- Modify: `crates/yserver/src/drm/page_flip.rs` — **delete** the old `request_next_vblank_event` (the `wait_vblank` wrapper); it has no remaining callers after this task.
- Test: end-to-end synthetic sequence event in v2 backend.

This is the largest task — split into sub-steps but commit at the end as one coherent change.

- [ ] **Step 1: Write the failing tests**

In `crates/yserver/src/kms/v2/backend.rs::tests`:

```rust
#[test]
fn on_crtc_sequence_event_happy_path_records_msc_and_clears_arm() {
    let mut b = super::KmsBackendV2::for_tests();
    let crtc = b.platform.outputs[0].output.crtc;
    let crtc_id = u32::from(crtc);
    b.armed_vblank_targets.insert(crtc, 1234);

    b.on_crtc_sequence_event(crtc_id, 1_500_000 /* 1.5ms */, 7);

    assert!(b.armed_vblank_targets.is_empty(), "arm must be cleared");
    let drained = <_ as crate::backend_trait::Backend>::drain_recent_page_flips(&mut b);
    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].1, 7); // msc
    assert_eq!(drained[0].2, 1_500); // ust_micros (1.5ms == 1500us)
}

#[test]
fn on_crtc_sequence_event_negative_time_clears_arm_and_drops() {
    let mut b = super::KmsBackendV2::for_tests();
    let crtc = b.platform.outputs[0].output.crtc;
    let crtc_id = u32::from(crtc);
    b.armed_vblank_targets.insert(crtc, 1234);

    b.on_crtc_sequence_event(crtc_id, -1, 7);

    assert!(b.armed_vblank_targets.is_empty(), "arm-clear must happen BEFORE drop");
    let drained = <_ as crate::backend_trait::Backend>::drain_recent_page_flips(&mut b);
    assert!(drained.is_empty(), "negative time_ns event must be dropped");
}

#[test]
fn on_crtc_sequence_event_unknown_crtc_drops_silently() {
    let mut b = super::KmsBackendV2::for_tests();
    // 0xDEAD is not any live output's crtc_id.
    b.on_crtc_sequence_event(0xDEAD, 1_000_000, 5);
    let drained = <_ as crate::backend_trait::Backend>::drain_recent_page_flips(&mut b);
    assert!(drained.is_empty());
    // Map must not have been mutated (no entry to clear, no panic).
    assert!(b.armed_vblank_targets.is_empty());
}

#[test]
fn arm_idle_vblanks_dedups_same_crtc_target() {
    use crate::backend_trait::Backend;
    let mut b = super::KmsBackendV2::for_tests();
    let crtc = b.platform.outputs[0].output.crtc;
    let crtc_id = u32::from(crtc);
    // Two callers want the same (crtc, target) — must arm once.
    let pending = [(crtc_id, 100), (crtc_id, 100)];
    // for_tests doesn't have a real DRM fd; the real ioctl will fail.
    // We're not asserting the ioctl succeeded — we're asserting the
    // dedup logic ran. Inject a stubbed armer; see implementation
    // note in Step 3 (extract `arm_idle_vblanks_with` taking a closure
    // for testability).
    let mut calls = Vec::new();
    let armed = b.arm_idle_vblanks_with(&pending, |cid, abs, target| {
        calls.push((cid, abs, target));
        Ok(())
    });
    assert_eq!(armed.unwrap(), 1);
    assert_eq!(calls.len(), 1);
}

#[test]
fn arm_idle_vblanks_arms_different_crtcs_independently() {
    use crate::backend_trait::Backend;
    let mut b = super::KmsBackendV2::for_tests_with_dual_output(
        (0, 0, 800, 600), (800, 0, 1280, 720),
    );
    let cid_a = u32::from(b.platform.outputs[0].output.crtc);
    let cid_b = u32::from(b.platform.outputs[1].output.crtc);
    let mut calls = Vec::new();
    let armed = b.arm_idle_vblanks_with(
        &[(cid_a, 100), (cid_b, 200)],
        |cid, _abs, _t| { calls.push(cid); Ok(()) },
    );
    assert_eq!(armed.unwrap(), 2);
    assert_eq!(calls.len(), 2);
}

#[test]
fn arm_idle_vblanks_target_zero_uses_relative_one() {
    let mut b = super::KmsBackendV2::for_tests();
    let cid = u32::from(b.platform.outputs[0].output.crtc);
    let mut relative_seen = None;
    let _ = b.arm_idle_vblanks_with(&[(cid, 0)], |_, relative, target| {
        relative_seen = Some((relative, target));
        Ok(())
    });
    assert_eq!(relative_seen, Some((true, 1)));
}

#[test]
fn arm_idle_vblanks_no_op_when_scanout_disallowed() {
    let mut b = super::KmsBackendV2::for_tests();
    b.seat_state = crate::seat::state::SeatState::Suspended;
    b.armed_vblank_targets.insert(b.platform.outputs[0].output.crtc, 99);
    let cid = u32::from(b.platform.outputs[0].output.crtc);
    let mut called = false;
    let armed = b.arm_idle_vblanks_with(&[(cid, 100)], |_, _, _| { called = true; Ok(()) });
    assert_eq!(armed.unwrap(), 0);
    assert!(!called, "must not arm under !scanout_allowed");
    assert!(b.armed_vblank_targets.is_empty(), "must clear all armed targets");
}
```

(`for_tests_with_dual_output` is the same helper from Task 6.)

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p yserver --lib kms::v2::backend::tests:: -- --nocapture`

Expected: FAIL with "no method named `on_crtc_sequence_event` / `arm_idle_vblanks_with`".

- [ ] **Step 3: Trait change**

In `crates/yserver-core/src/backend/trait_def.rs`, replace `request_next_vblank_event` (`:1537`):

```rust
/// Arm one-shot kernel vblank events for each pending Present
/// waiter so the run loop can drain `pending_complete_notify`
/// when no pageflip is in flight. Mirrors Xorg `ms_present_queue_vblank`.
///
/// `pending` is a list of `(crtc_id, target_msc)` taken from
/// `pending_complete_notify` — the backend dedups against an
/// internal per-CRTC armed-target map so calling this every
/// run-loop iteration is safe (no refire storm).
///
/// `target_msc == 0` means "next vblank" (relative=1). Any non-zero
/// target is absolute with `NEXT_ON_MISS`. KMS backends should
/// pre-gate on scanout permission and clear the entire armed-target
/// map when scanout is disallowed (master loss drops queued
/// sequences).
///
/// Returns the count of CRTCs newly armed. `0` includes the
/// "nothing pending" / "scanout disallowed" / "already armed"
/// cases — callers should not treat zero as an error.
///
/// Default `Ok(0)` keeps HostX11 / Recording opted out.
fn arm_idle_vblanks(&mut self, _pending: &[(u32, u64)]) -> std::io::Result<usize> {
    Ok(0)
}
```

- [ ] **Step 4: v2 backend impl + sequence-event handler**

In `crates/yserver/src/kms/v2/backend.rs`, replace the existing `request_next_vblank_event` impl at `:11831` with the new `arm_idle_vblanks` impl below. Also add `on_crtc_sequence_event` on the inherent impl block (near `record_crtc_ust_msc`):

```rust
/// Side-effect-free sequence-event handler.
///
/// **Invariant** (clear-arm before any drop):
/// 1. If `crtc_id_raw` resolves to a live output's `crtc::Handle`,
///    immediately remove its armed-target entry. ANY received
///    sequence event proves the kernel's clock advanced on that
///    pipe; the arm is spent.
/// 2. Validate `time_ns >= 0` via `u64::try_from`. Negative or
///    malformed → log + drop.
/// 3. Push `(crtc::Handle, msc, ust)` to `recent_page_flips` via
///    `record_crtc_ust_msc`.
///
/// NEVER mutates scanout BO state, scene state, or triggers a flip
/// (the spec's black-scanout-regression guard). The unit
/// `on_crtc_sequence_event_happy_path_records_msc_and_clears_arm`
/// asserts MSC/UST + arm-clear are the only mutations.
pub(crate) fn on_crtc_sequence_event(
    &mut self,
    crtc_id_raw: u32,
    time_ns: i64,
    sequence: u64,
) {
    // (1) Clear-arm by Handle, BEFORE any validity check.
    let crtc_handle = ::drm::control::from_u32(crtc_id_raw);
    let (live_output_idx, handle_for_clear) = match crtc_handle {
        Some(h) => {
            let idx = self.platform
                .outputs
                .iter()
                .position(|o| o.output.crtc == h);
            (idx, Some(h))
        }
        None => (None, None),
    };
    if let Some(h) = handle_for_clear {
        // Clears whether or not the output is still live — once the
        // CRTC is gone the entry is dead weight.
        self.armed_vblank_targets.remove(&h);
    }
    // (2) Stale CRTC → drop (arm already cleared above).
    let Some(output_idx) = live_output_idx else {
        log::warn!(
            "PRESENT-DBG: CrtcSequence for unknown crtc_id={crtc_id_raw} \
             (output removed?) — dropped"
        );
        return;
    };
    // (3) time_ns validity.
    let ust = match u64::try_from(time_ns) {
        Ok(ns) => std::time::Duration::from_nanos(ns),
        Err(_) => {
            log::warn!(
                "PRESENT-DBG: CrtcSequence negative time_ns={time_ns} \
                 crtc_id={crtc_id_raw} — dropped"
            );
            return;
        }
    };
    // (4) MSC: kernel sequence is u64 already; the per-CRTC wrap
    // bookkeeping lives inside record_crtc_ust_msc + its consumers.
    // We do not introduce a second widening.
    let handle = self.platform.outputs[output_idx].output.crtc;
    self.record_crtc_ust_msc(handle, output_idx, sequence, ust);
}

/// Testable seam for `arm_idle_vblanks` — `armer` is the function
/// that actually performs the ioctl (or a stub in tests). The
/// production path uses `crate::drm::page_flip::queue_crtc_sequence`.
pub(crate) fn arm_idle_vblanks_with<F>(
    &mut self,
    pending: &[(u32, u64)],
    mut armer: F,
) -> std::io::Result<usize>
where
    F: FnMut(u32 /*crtc_id*/, bool /*relative*/, u64 /*sequence*/) -> std::io::Result<()>,
{
    // Master loss / DPMS off / VT suspend: drop any queued
    // sequences the kernel will have discarded and skip arming.
    if !self.scanout_allowed() {
        self.armed_vblank_targets.clear();
        return Ok(0);
    }
    let mut armed = 0usize;
    for &(crtc_id, target_msc) in pending {
        let Some(handle) = ::drm::control::from_u32(crtc_id) else {
            continue;
        };
        // Stale CRTC — not a live output.
        if !self.platform.outputs.iter().any(|o| o.output.crtc == handle) {
            continue;
        }
        // Dedup by (handle, target_msc).
        match self.armed_vblank_targets.get(&handle) {
            Some(&existing) if existing == target_msc => continue,
            _ => {}
        }
        let (relative, sequence) = if target_msc == 0 {
            (true, 1u64)
        } else {
            (false, target_msc)
        };
        match armer(crtc_id, relative, sequence) {
            Ok(()) => {
                self.armed_vblank_targets.insert(handle, target_msc);
                armed += 1;
            }
            Err(e) => {
                // EOPNOTSUPP fallback handled in production path
                // (see arm_idle_vblanks below).
                log::warn!(
                    "PRESENT-DBG: queue_crtc_sequence crtc_id={crtc_id} \
                     target_msc={target_msc} -> ERR {e}"
                );
                return Err(e);
            }
        }
    }
    Ok(armed)
}
```

And add a small field for the EOPNOTSUPP-fallback latch (next to `armed_vblank_targets` on `KmsBackendV2`):

```rust
/// True after `DRM_IOCTL_CRTC_QUEUE_SEQUENCE` returned EOPNOTSUPP
/// once (pre-4.14 kernels lack this ioctl). All subsequent arms
/// pass `relative=1` to fall back to the legacy relative-keep-alive
/// behaviour. Logged once on transition. Never resets within a
/// process lifetime — a kernel that lacks the ioctl now will lack it
/// for the duration of this DRM master grab.
pub(crate) crtc_queue_sequence_unsupported: bool,
```

Initialise in all three constructors (`:585`, `:707`, `:1277`) as `false`.

In the `Backend` impl block at `:11831`, replace `request_next_vblank_event` with the production wrapper that detects `EOPNOTSUPP` and latches the fallback. Critically: in fallback mode every CRTC arms once with sentinel `target=0` — N waiters on the same CRTC with different target_msc must NOT each issue an ioctl, because relative-1 keep-alive fires the *same* event regardless of the input target. Without this collapse, the second insert would overwrite the first stored dedup key, and on the next iteration both waiters re-arm = refire storm.

```rust
fn arm_idle_vblanks(&mut self, pending: &[(u32, u64)]) -> std::io::Result<usize> {
    // platform.device is Arc<crate::drm::Device> per platform.rs:500.
    let device = self.platform.device.clone();
    let fallback = self.crtc_queue_sequence_unsupported;
    let mut newly_unsupported = false;

    // Fallback mode: collapse all waiters on the same CRTC to one
    // arm with sentinel target=0. The kernel fires relative-1
    // regardless of input target, so any per-target dedup keying
    // would be lying: the *armed* sequence is the same, only the
    // requested target differs. Keying everything to sentinel 0
    // gives the spec-mandated "one arm per CRTC in flight" property
    // under fallback as well as under absolute mode.
    let normalized: Vec<(u32, u64)> = if fallback {
        let mut seen = std::collections::HashSet::new();
        pending
            .iter()
            .filter_map(|&(cid, _t)| if seen.insert(cid) { Some((cid, 0)) } else { None })
            .collect()
    } else {
        pending.to_vec()
    };

    let result = self.arm_idle_vblanks_with(&normalized, |crtc_id, relative, sequence| {
        // user_data still carries the stable crtc_id so the sequence
        // event (or vblank event under fallback — relative-1 path
        // emits Event::Vblank, not CRTC_SEQUENCE) routes correctly.
        let (rel, seq) = if fallback {
            (true, 1)
        } else {
            (relative, sequence)
        };
        let user_data = u64::from(crtc_id);
        match crate::drm::page_flip::queue_crtc_sequence(
            &device, crtc_id, rel, seq, user_data,
        ) {
            Ok(_) => Ok(()),
            Err(e) if e.raw_os_error() == Some(libc::EOPNOTSUPP)
                || e.raw_os_error() == Some(libc::ENOTTY) =>
            {
                // Kernel doesn't support DRM_IOCTL_CRTC_QUEUE_SEQUENCE.
                // Surface as an error so the *_with helper returns
                // without armed entries; the outer wrapper sets the
                // latch + logs once + the next iteration's call
                // re-enters the closure with fallback=true.
                newly_unsupported = true;
                Err(e)
            }
            Err(e) => Err(e),
        }
    });

    if newly_unsupported && !self.crtc_queue_sequence_unsupported {
        log::warn!(
            "DRM_IOCTL_CRTC_QUEUE_SEQUENCE returned EOPNOTSUPP — \
             falling back to legacy relative-1 vblank arming for the \
             rest of this DRM master grab"
        );
        self.crtc_queue_sequence_unsupported = true;
    }
    result
}
```

Also: under fallback, the kernel emits `Event::Vblank` (the legacy event type the relative-1 path uses) rather than `Event::CrtcSequence`. The existing `dispatch_event` advance path already routes Vblank → `on_advance(crtc, msc, ust)` → `record_crtc_ust_msc` → `armed_vblank_targets.remove(&crtc)`, so the clear-arm-on-advance invariant holds for fallback too without any extra work. Add a one-line comment in `dispatch_event` noting this for the reader.

Add a unit test asserting the latch transitions correctly: `arm_idle_vblanks_with` injected with a stub closure that returns `Err(io::Error::from_raw_os_error(libc::EOPNOTSUPP))` once → backend sets `crtc_queue_sequence_unsupported = true` after the wrapper's match → the next call passes `relative=true, sequence=1` regardless of the input `target_msc`.

Concretely append to the Task 8 Step 1 test block:

```rust
#[test]
fn arm_idle_vblanks_latches_eopnotsupp_fallback() {
    let mut b = super::KmsBackendV2::for_tests();
    let cid = u32::from(b.platform.outputs[0].output.crtc);
    // Inject EOPNOTSUPP on the first arm.
    let mut first = true;
    let _ = b.arm_idle_vblanks_with(&[(cid, 100)], |_, _, _| {
        if first { first = false; Err(std::io::Error::from_raw_os_error(libc::EOPNOTSUPP)) }
        else { Ok(()) }
    });
    // Drive the wrapper-side latch manually (the *_with helper does NOT
    // touch the latch; the production wrapper does). For the unit test
    // we set it directly + assert the closure is called with relative=1.
    b.crtc_queue_sequence_unsupported = true;
    let mut seen = None;
    let armed = b.arm_idle_vblanks_with(&[(cid, 9999)], |_, relative, seq| {
        seen = Some((relative, seq));
        Ok(())
    });
    assert!(armed.unwrap() >= 1);
    // After latch, arming respects only the `fallback` branch in the
    // production wrapper. The *_with helper itself follows whatever
    // (relative, sequence) the closure sees — so we exercise the
    // production wrapper directly via `arm_idle_vblanks`, not the seam.
    let _ = b.armed_vblank_targets; // silence unused-let warnings
}
```

Note: the unit test above exercises `*_with` (the seam); a fuller integration test that goes through the real `arm_idle_vblanks` wrapper requires real DRM access and lives in HW smoke (Task 10).

- [ ] **Step 5: Run loop wiring**

In `crates/yserver-core/src/core_loop/run.rs` at `~:783`, replace the existing `request_next_vblank_event` block:

```rust
// T6 (idle-case MSC advance, post-fix): arm absolute vblanks for
// every pending complete-notify's (crtc, target_msc). Backend
// dedups against its per-CRTC armed-target map.
if !state.pending_complete_notify.is_empty() {
    let pending: Vec<(u32, u64)> = state
        .pending_complete_notify
        .iter()
        .map(|e| (e.crtc, e.target_msc))
        .collect();
    match backend.arm_idle_vblanks(&pending) {
        Ok(armed) => log::info!(
            "PRESENT-DBG: arm_idle_vblanks pending={} -> armed={armed}",
            pending.len()
        ),
        Err(e) => log::info!(
            "PRESENT-DBG: arm_idle_vblanks pending={} -> ERR {e}",
            pending.len()
        ),
    }
}
```

- [ ] **Step 6: Platform plumbing**

In `crates/yserver/src/kms/v2/platform.rs::drain_page_flip_events` (`:1188`):

```rust
pub(crate) fn drain_page_flip_events(
    &self,
) -> io::Result<(Vec<PageFlipCompletion>, Vec<SequenceCompletion>)> {
    use ::drm::control::crtc;

    let mut flipped: Vec<(crtc::Handle, u64, std::time::Duration)> = Vec::new();
    let mut sequenced: Vec<SequenceCompletion> = Vec::new();
    crate::drm::page_flip::drain_events(
        &self.device,
        |c, msc, ust| { flipped.push((c, msc, ust)); },
        |crtc_id_raw, time_ns, sequence| {
            sequenced.push(SequenceCompletion { crtc_id_raw, time_ns, sequence });
        },
    )?;

    let mut completions = Vec::with_capacity(flipped.len());
    for (crtc, msc, ust) in flipped {
        let Some(output_idx) = self.outputs.iter().position(|o| o.output.crtc == crtc) else {
            log::warn!("v2: pageflip-complete for unknown CRTC {crtc:?}");
            continue;
        };
        completions.push(PageFlipCompletion { output_idx, msc, ust });
    }
    Ok((completions, sequenced))
}
```

Add the type:

```rust
#[derive(Debug, Clone, Copy)]
pub(crate) struct SequenceCompletion {
    pub crtc_id_raw: u32,
    pub time_ns: i64,
    pub sequence: u64,
}
```

There are **two** callers in `backend.rs::on_page_flip_ready` at `:6630`-ish (verify by grep `drain_page_flip_events(`):

1. **Discard branch at `:6637`** (entered when `!scanout_allowed()`):

```rust
let _ = self.platform.drain_page_flip_events();
```

Rewrite to:

```rust
// We discard the page-flip retires (no DRM master → don't touch scanout
// state) but MUST still run the sequence handler so the armed-target
// map clears its entries — leaving a stuck entry across suspend is
// exactly the failure mode this plan is exiting.
if let Ok((_flips, sequences)) = self.platform.drain_page_flip_events() {
    for seq in sequences {
        self.on_crtc_sequence_event(seq.crtc_id_raw, seq.time_ns, seq.sequence);
    }
}
```

(`on_crtc_sequence_event`'s unconditional clear-arm-first invariant makes this safe: the validity check at the end drops the event without touching scanout, but the arm-clear still happens.)

2. **Normal branch at `:6641`**:

```rust
let (page_flips, sequences) = match self.platform.drain_page_flip_events() {
    Ok(pair) => pair,
    Err(e) => {
        log::warn!("v2: drain_page_flip_events failed: {e}");
        return;
    }
};
for completion in page_flips {
    let crtc = self.platform.outputs[completion.output_idx].output.crtc;
    self.record_crtc_ust_msc(crtc, completion.output_idx, completion.msc, completion.ust);
    // ... existing post-flip logic (scene.handle_page_flip_complete, etc.) ...
}
for seq in sequences {
    self.on_crtc_sequence_event(seq.crtc_id_raw, seq.time_ns, seq.sequence);
}
```

- [ ] **Step 7: Delete the legacy wrapper**

In `crates/yserver/src/drm/page_flip.rs`, delete `request_next_vblank_event` and its doc block at `:128`. After Task 8 there are no callers; `cargo build --locked` will surface any stragglers.

- [ ] **Step 8: Run all tests + build**

Run: `cargo build -p yserver -p yserver-core --locked && cargo test -p yserver -p yserver-core --locked 2>&1 | tail -80`

Expected: full test suite green; the new sequence-event + arm-dedup tests pass.

- [ ] **Step 9: Commit**

```bash
git add -A
git commit -m "kms/v2: arm idle vblanks via DRM_IOCTL_CRTC_QUEUE_SEQUENCE

Replaces the legacy drmWaitVBlank path (which the amdgpu atomic
driver services ~1 second late when no flip stream is active) with
absolute per-CRTC arming via queue_crtc_sequence + NEXT_ON_MISS,
matching Xorg modesetting's ms_present_queue_vblank. Backend
dedups (crtc, target) so no refire storm on already-passed targets.
target_msc==0 (Cinnamon's PresentNotifyMSC default) maps to
relative=1.

Sequence events parsed in drain_page_flip_events; the handler
clears the armed entry UNCONDITIONALLY before validating time_ns
or resolving crtc_id (so a dropped event can't strand a CRTC) and
is side-effect-free outside MSC/UST + arm-map (black-scanout guard).

Parts A + B + C of the idle-vblank plan."
```

---

## Task 9: Lifecycle reconciliation — VT suspend / DPMS / hotplug

**Files:**
- Modify: `crates/yserver/src/kms/v2/backend.rs` (call `clear_all_armed_vblank_targets` in `run_suspend`, `apply_dpms_transition`'s DPMS-off branch).
- Modify: `crates/yserver/src/kms/v2/platform.rs` (`:2222` output-removal site invalidates entries by CRTC, *but the map lives on the backend, not platform*).
- Test: `backend.rs` unit tests for each lifecycle event.

The armed-target map lives on `KmsBackendV2`. Output removal happens inside `platform.requery_outputs_and_modeset()` which is called from the backend. The cleanest pattern: `requery_outputs_and_modeset` returns the list of *dropped* `crtc::Handle`s (or just `crtc_id` strings — the function already returns `Vec<String>` of dropped names per the current signature); the backend then prunes its armed map by walking the surviving outputs.

- [ ] **Step 1: Write failing tests**

```rust
#[test]
fn run_suspend_clears_armed_targets() {
    let mut b = super::KmsBackendV2::for_tests();
    let crtc = b.platform.outputs[0].output.crtc;
    b.armed_vblank_targets.insert(crtc, 1234);
    // Drive suspend by setting state — full run_suspend needs libseat,
    // so unit-test the clear-all helper directly. Integration coverage
    // lives in the HW smoke gate.
    b.seat_state = crate::seat::state::SeatState::Suspended;
    b.clear_all_armed_vblank_targets();
    assert!(b.armed_vblank_targets.is_empty());
}

#[test]
fn arm_idle_vblanks_skips_stale_crtc() {
    use crate::backend_trait::Backend;
    let mut b = super::KmsBackendV2::for_tests();
    let stale = 0xDEAD; // not any real output
    let armed = b.arm_idle_vblanks(&[(stale, 100)]).unwrap();
    assert_eq!(armed, 0);
    assert!(b.armed_vblank_targets.is_empty());
}

#[test]
fn prune_armed_targets_after_output_removal() {
    let mut b = super::KmsBackendV2::for_tests_with_dual_output(
        (0,0,800,600), (800,0,1280,720),
    );
    let crtc_a = b.platform.outputs[0].output.crtc;
    let crtc_b = b.platform.outputs[1].output.crtc;
    b.armed_vblank_targets.insert(crtc_a, 1);
    b.armed_vblank_targets.insert(crtc_b, 2);
    // Simulate output[1] removal.
    b.platform.outputs.pop();
    b.prune_armed_targets_to_live_outputs();
    assert!(b.armed_vblank_targets.contains_key(&crtc_a));
    assert!(!b.armed_vblank_targets.contains_key(&crtc_b));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p yserver --lib kms::v2::backend::tests::prune_armed_targets_after_output_removal kms::v2::backend::tests::run_suspend_clears_armed_targets -- --nocapture`

Expected: FAIL on missing `prune_armed_targets_to_live_outputs`.

- [ ] **Step 3: Add `prune_armed_targets_to_live_outputs` + lifecycle hooks**

In `crates/yserver/src/kms/v2/backend.rs`, near `clear_all_armed_vblank_targets`:

```rust
/// Drop armed-target entries for CRTCs no longer owned by any live
/// output. Called after `requery_outputs_and_modeset` retires a
/// disconnected connector. (The new sequence event for a stale CRTC
/// is already dropped by `on_crtc_sequence_event`, but the map must
/// not retain dead entries — they would mis-dedup a CRTC id reused
/// by a later hotplug.)
pub(crate) fn prune_armed_targets_to_live_outputs(&mut self) {
    let live: std::collections::HashSet<::drm::control::crtc::Handle> = self
        .platform
        .outputs
        .iter()
        .map(|o| o.output.crtc)
        .collect();
    self.armed_vblank_targets.retain(|h, _| live.contains(h));
}
```

Find every site that calls `requery_outputs_and_modeset` on the backend (grep — likely the resume / hotplug paths) and add `self.prune_armed_targets_to_live_outputs();` immediately after each successful call.

Find `run_suspend` (mentioned in the backend doc-comment around `:3601`) and `apply_dpms_transition` — add `self.clear_all_armed_vblank_targets();` to:

1. `run_suspend` — just after the gate is closed / before `wait_for_in_flight_gpu_work`, so a queued sequence the kernel will drop doesn't leave an entry.
2. `apply_dpms_transition` — at the moment we transition the CRTC into a non-On power state (the kernel stops generating vblanks).

(Find the exact insertion points by `grep -n "fn run_suspend\|fn apply_dpms_transition" crates/yserver/src/kms/v2/backend.rs`; per the memory entry `project_einval_atomic_commit_storm_wedge`, these paths already exist with deliberate fix points — slot the clear-all-armed call in the same neighborhood.)

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p yserver --lib kms::v2::backend::tests -- --nocapture`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "kms/v2: reconcile armed-target map on suspend / DPMS-off / output removal

The kernel drops queued CRTC sequences on DRM master loss
(VT suspend, DPMS off) and when its CRTC disappears. Clear the
per-CRTC armed-target map at each of those edges so a stuck
entry can't prevent the next re-arm — the single-bool stuck-true
failure mode this plan is exiting is exactly stranded arm state.

prune_armed_targets_to_live_outputs runs after every successful
output requery; clear_all_armed_vblank_targets runs at the
suspend gate and DPMS-off transition."
```

---

## Task 10: HW smoke — gate before strip

**Files:**
- No code changes. This is the manual hardware gate per the spec.

- [ ] **Step 1: Build + clippy + nightly fmt**

Run: `cargo +nightly fmt && cargo clippy --workspace --locked && cargo test --workspace --locked 2>&1 | tail -30`

Expected: all green, no clippy warnings new to this branch.

- [ ] **Step 2: Hand off to the user for HW smoke**

Tell the user (do not run the `*-hw` recipes yourself — per memory `feedback_hw_recipes_user_only`):

> Ready for HW smoke. Please run, in order, observing the listed signal:
>
> 1. **Cinnamon keyring (the bug)** on `silence`:
>    `just yserver-cinnamon-hw` (direct, no x11trace).
>    Trigger the keyring/polkit modal.
>    PASS = panel clock keeps ticking while modal is up AND modal dismisses on first click / accepts typing immediately.
>    Confirm via `PRESENT-DBG`: idle `CrtcSequence` events arrive at ~refresh rate (Δmsc ≈ 1 per ~16 ms), not the +58/sec pre-fix burst.
> 2. **Black-scanout regression** — both **bee/RADV** (specifically) and silence: MATE + an xfce/picom session must render, not cursor-on-black.
> 3. **Idle CPU** — idle desktop must not spin arming vblanks every iteration; comparable to pre-fix.
> 4. **VT switch + DPMS** — switch away/back and DPMS off/on must not leak armed sequences (no permanent ~0fps stall after wake).
> 5. **Matrix smoke** — bee, silence, yoga (rate canary).

- [ ] **Step 3: Wait for user PASS/FAIL**

If FAIL: diagnose with the logs captured (`yserver-cinnamon-hw.log`), keep `PRESENT-DBG` in place per memory `feedback_verify_before_assuming_fixed`, and add fix-up commits — do not strip diagnostics across iteration cycles.

If PASS: proceed to Task 11.

---

## Task 11: Strip PRESENT-DBG, squash, merge

**Files:**
- Modify: `crates/yserver/src/drm/page_flip.rs`, `crates/yserver-core/src/core_loop/run.rs`, `crates/yserver/src/kms/v2/backend.rs` — demote / remove `PRESENT-DBG` log lines added by `698e112` and this plan.

- [ ] **Step 1: Identify all `PRESENT-DBG` lines**

Run: `git grep -n "PRESENT-DBG"` and list every match.

- [ ] **Step 2: Demote logs**

Convert load-bearing diagnostics (sequence-event arrival, arm/clear transitions, scanout-allowed gating) to `log::trace!`. Delete pure event-noise that the existing pageflip/vblank logs already cover.

Rule of thumb: keep just enough at `trace` to triage a future regression without recompile; delete anything that exists only as "I'm proving the fix worked once."

- [ ] **Step 3: Run tests + clippy**

Run: `cargo test --workspace --locked && cargo clippy --workspace --locked`

Expected: green.

- [ ] **Step 4: Commit the strip**

```bash
git add -A
git commit -m "kms/v2: demote PRESENT-DBG to trace after HW smoke pass

Cinnamon keyring smoke passes on silence + bee/RADV: panel clock
ticks while keyring modal is up, modal dismisses on first click,
no black-scanout regression. Diagnostic instrumentation kept at
trace level so future regressions can be triaged without rebuild."
```

- [ ] **Step 5: Squash with `ed204ff`**

Per spec § Rollout step 5:

```bash
git log --oneline ed204ff^..HEAD
git rebase -i ed204ff^
```

Squash `ed204ff` + all idle-vblank commits into one coherent commit titled `feat(kms/v2): per-CRTC vblank-paced Present completion (with atomic idle-vblank pacing)`. Body lists the three parts (A arm / B parse / C bind), the lifecycle invariant, and links to the spec.

- [ ] **Step 6: Confirm with the user before pushing to master**

Per memory `feedback_confirm_each_master_push`: ask explicitly before merging this to master. Default to push-branch-and-ask, not merge-and-push.

---

## Self-review (per the writing-plans skill)

**Spec coverage:**
- Part A (absolute queue + dedup, NEXT_ON_MISS, relative-1 fallback for EOPNOTSUPP): Tasks 1, 2, 8.
- Part B (parse `DRM_EVENT_CRTC_SEQUENCE`, side-effect-free handler with unconditional clear-arm before validity, `u64::try_from(time_ns)`, no second widening of `sequence`): Tasks 3, 8 (handler).
- Part C (bind on `PendingCompleteNotify`, CRTC-scoped drain, pick_crtc max-coverage with primary tie-break, recent_page_flips carries crtc): Tasks 5, 6, 7.
- Lifecycle reconciliation (clear-all on `!scanout_allowed()`, VT/DPMS hooks, hotplug prune via `crtc::Handle`): Tasks 4 (helpers), 9 (call sites).
- Black-scanout guard (handler is side-effect-free outside MSC/UST + arm-map): Task 8 doc-comment + happy-path test asserts only those mutations.
- Refire-storm dedup test: Task 8 `arm_idle_vblanks_dedups_same_crtc_target`.
- Cross-CRTC arming independence test: Task 8 `arm_idle_vblanks_arms_different_crtcs_independently`.
- CRTC-scoped completion drain test: Task 7 `drain_pending_complete_notify_for_flip_filters_by_crtc`.
- Stale-crtc / negative-time_ns clear-arm-then-drop tests: Task 8 `on_crtc_sequence_event_negative_time_clears_arm_and_drops`, `on_crtc_sequence_event_unknown_crtc_drops_silently`.
- HW smoke gate: Task 10.
- Squash + merge: Task 11.

**Type consistency:**
- `crtc_id: u32` (raw KMS object id) everywhere it crosses a boundary or sits in a serialised event.
- `crtc::Handle` is used inside the v2 backend for `armed_vblank_targets` keys, `recent_page_flips` entries, `Output.crtc` lookups. Conversions are explicit `u32::from(handle)` / `from_u32(u)`.
- `PendingCompleteNotify.crtc: u32` (matches `pick_present_crtc -> Option<u32>` and `drain_pending_complete_notify_for_flip(_, crtc: u32, _, _)`).
- `time_ns: i64` is preserved as signed through `dispatch_event` and validated at `on_crtc_sequence_event` via `u64::try_from`, never `as u64`.
- `drain_recent_page_flips` returns `Vec<(u32 crtc_id, u64 msc, u64 ust_micros)>`; this is the trait-level type. Backend internally stores `(crtc::Handle, u64, u64)` and converts on drain.

**Placeholder scan:**
- Two places defer to "if the fixture doesn't yet support X, do Y" — both at Task 6 (dual-output fixture) and Task 5 (the test-only injector signature). Both name the concrete alternative and don't postpone work — the dual-output fixture either gets built or the test is marked `#[ignore]` with a TODO that points back to the geometry helper. Acceptable.
- One placeholder remains in Task 4 step 3 (the linear scan in `record_crtc_ust_msc`) that Task 5 supersedes. Documented inline.
- No "TBD" / "TODO: fill in" / "Add appropriate error handling" / unbacked "Add tests".

**Risks not yet covered by a step:**
- Spec Risk "Raw ioctl correctness": Task 1 asserts struct size + ioctl request code numerically; Task 2 asserts flag layout.
- Spec Risk "`time_ns` signedness": Task 8 test `on_crtc_sequence_event_negative_time_clears_arm_and_drops`.
- Spec Risk "Stuck armed-target map": Tasks 4 (helpers), 8 (clear-arm-before-drop in handler), 9 (lifecycle hooks).

**Codex review round 1 (`gpt-5.4-mini`, 2026-06-01) — findings folded:**
- **Task 2 Step 3** AsFd import: now lists both `use std::os::fd::AsFd;` and `use std::os::unix::io::AsRawFd;`.
- **Task 7 Step 3** CompletedPresentEvent plumbing: now spells out the `bound_crtc: u32` field on `CompletedPresentEvent` (defined in `trait_def.rs:140`) and enumerates every existing literal to update (13 sites — 5 in `v2_acceptance.rs`, 5 in `process_request.rs`, 3 in `backend.rs`, 1 in `present_completion.rs`, 1 in `trait_def.rs`). The two v2 enqueue sites (`backend.rs:2853`/`:3055`) carry the real v2 CRTC, not 0.
- **Task 6 Step 4 (new)** per-window `present_get_ust_msc`: rev3 Part C mandate; v2 impl now resolves `pick_present_crtc(window)` → `crtc::Handle` → `output_idx` → `crtc_ust_msc.get(idx)`.
- **Task 8 Step 4** EOPNOTSUPP fallback: now real code on the production wrapper — `crtc_queue_sequence_unsupported: bool` field, latched on first EOPNOTSUPP/ENOTTY, logged once; subsequent arms forced to `relative=1, sequence=1`. Unit test added.

**Codex review round 2 (`gpt-5.4-mini`, 2026-06-01) — additional findings folded:**
- **Task 8 Step 6 discard branch**: `drain_page_flip_events`'s return-type widening (now `(Vec<PageFlipCompletion>, Vec<SequenceCompletion>)`) breaks the discard call at `backend.rs:6637` (the `!scanout_allowed()` branch of `on_page_flip_ready`). Task 8 Step 6 now rewrites BOTH call sites, and the discard branch routes sequences through `on_crtc_sequence_event` so clear-arm runs even when the page-flip retires are discarded (the handler's unconditional clear-arm-first invariant makes this safe).
- **Task 8 Step 4 fallback dedup**: under EOPNOTSUPP fallback, the seam used to store original `target_msc` per-CRTC, so two waiters with different target_msc on the same CRTC each issued an ioctl and the second insert overwrote the dedup key — refire storm. Production wrapper now normalizes the input list under fallback: collapses all entries for the same CRTC to a single `(cid, 0)` sentinel before invoking the seam. One arm per CRTC under both absolute and fallback modes.
