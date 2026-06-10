# Animated cursors (RENDER CreateAnimCursor) — design

**Date:** 2026-06-10
**Status:** approved scope: KMS v2 backend only
**Branch:** `feat/anim-cursor`

## Problem

RENDER `CreateAnimCursor` (opcode 31) is handled in
`crates/yserver-core/src/core_loop/process_request.rs:1880-1960` as a
*static degeneration*: the frame list is parsed and validated, but the
new cursor permanently inherits the FIRST sub-cursor's host handle.
Delays are ignored. Users see a frozen first frame instead of a
spinner (`left_ptr_watch`, busy cursors during app launch, etc.).

## Goal

Real frame cycling on the KMS v2 backend (the dogfooding target),
honoring per-frame delays. ynest keeps the current static-first-frame
behavior unchanged (explicit decision 2026-06-10; revisit later if
needed).

## Non-goals

- ynest animation (host-forwarding) — deferred.
- Per-device animation state (Xorg keeps anim state per
  `DeviceIntPtr`; yserver has a single effective pointer cursor).
- Drift-corrected absolute scheduling — spinners don't need it.

## Reference: how Xorg does it

`render/animcur.c`: an animated cursor stores `nelt` ×
`AnimCurElt { pCursor, delay_ms }`. The `DisplayCursor` screen-proc
wrapper detects an animated cursor becoming current, shows frame 0,
and arms an OS timer. The timer callback displays
`(elt + 1) % nelt` via the *unwrapped* DisplayCursor and returns the
next frame's delay (self-rearming). Constituent cursors are
refcounted (`RefCursor`) so the client may free them after creation.
`ProcRenderCreateAnimCursor` (`render/render.c:1783`) errors:
`BadLength` on odd request length, `BadValue` on zero frames,
`BadCursor` on unknown sub-cursor, `BadMatch` on nested animated
cursors.

## Design

### Architecture choice

Backend-internal animation, driven by the existing
`Backend::next_wakeup()` deadline that already feeds the core loop's
poll timeout (`core_loop/run.rs:409-437`). The KMS backend already
owns effective-cursor resolution (`refresh_effective_cursor`,
`kms/v2/backend.rs:1122`), the HW cursor-plane upload path, and the
scene cursor registration — animation is a backend concern there.

Rejected alternative: core-driven ticking (core stores frames, calls
`define_cursor` per tick). More Xorg-shaped and would extend to ynest
for free, but core does not know which cursor is *effective* — that
knowledge lives in the KMS backend — so it would need new plumbing
for no benefit at the approved scope.

### New backend trait method

```rust
/// RENDER::CreateAnimCursor. `frames` pairs each sub-cursor's host
/// handle with its delay in ms. Returns the new animated cursor's
/// handle, or None when the backend does not animate (caller falls
/// back to static degeneration on frame 0).
fn create_anim_cursor(
    &mut self,
    _origin: Option<OriginContext>,
    _frames: &[(CursorHandle, u32)],
) -> io::Result<Option<CursorHandle>> {
    Ok(None)
}
```

Default impl returns `None` → the existing handler path (first
sub-cursor's handle) stays as the fallback. ynest and the recording
backend need no changes.

### Request handler change

`process_request.rs` CreateAnimCursor handler: after the existing
validation (unchanged — client-owned fresh id, non-empty 8-byte-
aligned list, every sub-cursor exists), call
`backend.create_anim_cursor(...)`. On `Some(handle)` store that as
the cursor's `host_xid`; on `None` keep today's first-frame path.

Validation gap to close while here (matches Xorg): reject a
sub-cursor that is itself animated with `BadMatch` (Xorg refuses
nested animated cursors; we currently would silently treat the
nested cursor as one frame).

### KMS backend: data model

```rust
pub(crate) struct AnimCursorRecord {
    /// Frame snapshots taken at creation time. Holding the Arcs
    /// makes sub-cursor lifetime a non-issue: the client may free
    /// the constituent cursors immediately (Xorg refcounts them; we
    /// snapshot instead).
    pub(crate) frames: Vec<(std::sync::Arc<CursorRecord>, std::time::Duration)>,
}
```

Stored in a new `anim_cursor_records: HashMap<u32 /*xid*/,
AnimCursorRecord>` beside the existing
`cursor_records: HashMap<u32, Arc<CursorRecord>>`
(`kms/v2/backend.rs:274`). The animated cursor also gets an entry in
`cursor_records` pointing at frame 0's record so every existing
"static cursor" code path (effective-cursor walk, XFixes, scene
registration) works untouched for frame 0.

`create_anim_cursor` impl: look up each sub-cursor xid in
`cursor_records` (missing xid → `io::Error` mapped to the handler's
existing error path), clone the Arcs, allocate a fresh cursor xid the
same way `create_cursor` does, insert both maps. A delay of 0 ms is
clamped to 16 ms (degenerate spin-loop guard; Xorg lets 0 through and
busy-loops the timer — we don't).

### Animation state (one active animation)

```rust
struct ActiveCursorAnim {
    xid: u32,            // animated cursor whose frames are cycling
    frame: usize,        // current index
    next_frame: Instant, // deadline for the next advance
}
```

One `Option<ActiveCursorAnim>` on the backend — matches the
single-effective-cursor model.

- `refresh_effective_cursor()` resolves the effective cursor; if its
  xid has an `AnimCursorRecord`, set/replace `ActiveCursorAnim`
  (frame 0, `now + delay[0]`). If the effective cursor is not
  animated, clear it. Re-resolving to the *same* animated cursor must
  NOT restart the animation (Xorg: "already current → do nothing");
  only a change of effective cursor xid resets to frame 0.
- Free of the animated cursor (`free_cursor`) drops both map entries
  and clears `ActiveCursorAnim` if it points at that xid.

### Frame tick

- `next_wakeup()` (`kms/v2/backend.rs:8217`) additionally chains
  `ActiveCursorAnim.next_frame` — but only when outputs are active
  (see gating below).
- The tick runs in the backend's wakeup/housekeeping path that the
  core loop already drives after poll returns: if
  `now >= next_frame`, advance `frame = (frame + 1) % n`, then push
  the new frame's `Arc<CursorRecord>` through the existing
  cursor-changed path — same code `refresh_effective_cursor` uses:
  scene `register_cursor` (SW path) or
  `queue_steady_state_cursor_upload()` (HW plane path, ≤64×64 memcpy
  into the dumb buffer). Re-arm `next_frame = now + delay[frame]`
  (relative re-arm, self-heals after a stall; if multiple frames'
  deadlines were missed while stalled, advance once and re-arm — do
  not fast-forward through missed frames).
- Implementation detail: frame swap must bump through the existing
  `CursorRecord.version` dedup mechanism so the upload path sees a
  change. Each frame's record already has a distinct version
  (created by separate `create_cursor` calls), so swapping Arcs
  suffices; verify in tests.

### DPMS / VT gating

Frame ticks and uploads are gated on `kms_outputs_active` and
`scanout_allowed()` exactly like `maybe_composite` (EINVAL-storm
lesson, 2026-05-30): while outputs are off or VT is switched away,
`next_wakeup()` does not report the anim deadline (no wakeups burned
on an invisible cursor) and no uploads happen. On wake/VT-return the
deadline re-arms from `now` — first frame advance happens one delay
after resume.

### XFixes GetCursorImage

Must return the *current frame* (Xorg behavior). The KMS paths that
serve cursor images out of `cursor_records`
(`kms/v2/backend.rs:4167,14030,14390`) read the animated cursor's
`cursor_records` entry — so the tick also updates
`cursor_records[anim_xid]` to the current frame's Arc. That one
update keeps every existing reader (XFixes, scene, HW upload)
frame-correct without new branches.

### Error handling

- Handler validation errors: unchanged (`BadCursor` for unknown
  sub-cursor, length errors), plus new `BadMatch` for nested
  animated cursors.
- `create_anim_cursor` backend failure (sub-cursor record missing —
  "can't happen" after handler validation): propagate as the
  handler's existing backend-error path; no partial state (insert
  into maps only after all lookups succeed).

## Testing

1. **Unit (KMS backend, no display):** existing test harness at
   `kms/v2/backend.rs:16476+` style —
   - create 3 cursors, create anim cursor → `cursor_records[anim]`
     is frame 0; `next_wakeup()` includes the deadline.
   - simulate tick past deadline → frame advances, wraps mod n,
     `cursor_records[anim]` follows, versions differ across ticks.
   - effective cursor switches away → anim cleared, `next_wakeup()`
     no longer reports it; switch back → restarts at frame 0;
     re-resolve to same cursor → frame index preserved.
   - free anim cursor mid-animation → no dangling state.
   - sub-cursors freed after creation → frames still cycle
     (Arc snapshot).
   - delay 0 clamped.
   - nested anim cursor → handler returns `BadMatch`.
2. **vng smoke (iteration signal, not release gate):** small xcb
   test client that builds a 2-frame anim cursor with visually
   distinct frames + 200 ms delays and sets it on a window; verify
   visibly cycling. `left_ptr_watch` via a busy GTK app as a
   real-world check.
3. **HW dogfood (bee, MATE):** busy cursor during app launch spins;
   DPMS off/on while spinner active → no EINVAL storm, animation
   resumes. HW runs coordinated with the user per the established
   tmux procedure.

## Risks

- HW cursor plane re-upload cadence: a memcpy per frame at 30–100 ms
  is negligible, but the upload path was built for rare cursor
  changes — watch for log spam or per-upload allocations; the
  version-dedup mechanism must not treat alternating frames as
  "unchanged".
- Two maps keyed by the same xid must stay in sync on free/replace
  (same discipline as the existing `cursor_records` sibling-map
  comment at `backend.rs:277`).
