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

`process_request.rs` CreateAnimCursor handler keeps the existing
id-ownership and sub-cursor-exists checks, then calls
`backend.create_anim_cursor(...)`. On `Some(handle)` store that as
the cursor's `host_xid`; on `None` keep today's first-frame path.

Two validation gaps to close while here (today's handler returns a
silent `Handled` for both — `process_request.rs:1922-1928`):

- empty frame list → `BadValue` (Xorg: `ncursor <= 0`,
  `render.c:1801`); list length not a multiple of 8 bytes →
  `BadLength` (Xorg: odd `req_len`, `render.c:1796`).
- a sub-cursor that is itself animated → `BadMatch` (Xorg refuses
  nested animated cursors, `animcur.c:316`). The core resource
  table does not currently track animatedness, so the core `Cursor`
  resource (`resources.rs:2142`) gains an `anim: bool` flag, set on
  **every** successful CreateAnimCursor — on all backends, including
  when the backend returned `None` and the cursor degenerated to
  frame 0. The handler checks the flag on each sub-cursor; no
  backend round-trip. This means ynest now also rejects nested
  animated cursors with `BadMatch` where it previously accepted
  them — a deliberate protocol-fidelity fix (Xorg rejects them),
  distinct from the "ynest keeps static rendering" non-goal.

### KMS backend: data model

Namespace note (these are different `u32` spaces — do not mix):
client cursor *xids* live in the core resource table, which maps
them to backend-allocated *host handles* (`CursorHandle`). All KMS
cursor maps — `cursor_records`, `cursor_pixmaps`,
`effective_cursor_xid` — are keyed by **host handle** raw values
(`kms/v2/backend.rs:274-278`). Everything below is in host-handle
space; "handle" means host handle.

```rust
pub(crate) struct AnimCursorRecord {
    /// Frame snapshots taken at creation time: the frame's record,
    /// its sprite pixmap (what the scene samples in SW-cursor
    /// mode), and its delay.
    pub(crate) frames: Vec<AnimFrame>,
}

pub(crate) struct AnimFrame {
    pub(crate) record: std::sync::Arc<CursorRecord>,
    pub(crate) pixmap: crate::kms::v2::store::DrawableId,
    pub(crate) delay: std::time::Duration,
}
```

Stored in a new `anim_cursor_records: HashMap<u32 /*handle*/,
AnimCursorRecord>` beside `cursor_records` / `cursor_pixmaps`. The
animated cursor also gets entries in `cursor_records` and
`cursor_pixmaps` pointing at frame 0's record/pixmap so every
existing "static cursor" code path (effective-cursor walk, XFixes,
scene registration) works untouched for frame 0.

`create_anim_cursor` impl: look up each sub-cursor handle in
`cursor_records` AND `cursor_pixmaps` (missing → `io::Error` mapped
to the handler's existing error path; insert nothing on partial
failure), clone the Arcs/ids, allocate a fresh handle the same way
`create_cursor` does, insert all three maps.

Frame lifetime: KMS v2 keeps the trait's default no-op `free_cursor`
(`trait_def.rs:944`) for animated handles too — backend-side cursor
records and sprite pixmaps are never freed (status quo). This is
deliberate, not an omission: window/root cursor bindings store raw
host handles (`backend.rs:92,10052`), and core's `FreeCursor` removes
only the resource-table entry — a freed animated cursor can still be
bound and *effective*, and per X11 semantics (Xorg refcounting) it
must keep displaying — and keep animating — until unreferenced.
Eagerly removing the maps would strand the effective cursor on
missing entries. The no-op gives the keep-alive half of Xorg's
refcounting for free; the missing release half is the same
pre-existing leak static cursors already have — out of scope.

Delay of 0 ms is clamped to 16 ms. **Explicit Xorg deviation:** Xorg
stores 0 as-is (`animcur.c:359`) and lets the timer re-fire
immediately; in our deadline-driven loop a 0 delay would make
`next_wakeup()` return `now` forever — a poll busy-spin. 16 ms ≈ one
display frame; no client-visible behavior difference at that rate.

### Animation state (one active animation)

```rust
struct ActiveCursorAnim {
    handle: u32,         // animated cursor (host handle) cycling
    frame: usize,        // current index
    next_frame: Instant, // deadline for the next advance
}
```

One `Option<ActiveCursorAnim>` on the backend — matches the
single-effective-cursor model.

- `refresh_effective_cursor()` resolves the effective cursor; if its
  handle has an `AnimCursorRecord`, set/replace `ActiveCursorAnim`
  (frame 0, `now + delay[0]`). If the effective cursor is not
  animated, clear it. Re-resolving to the *same* animated cursor must
  NOT restart the animation (Xorg: "already current → do nothing");
  only a change of effective cursor handle resets to frame 0. (The
  existing early-return at `backend.rs:1125` when the handle is
  unchanged already gives this.)
- Client `FreeCursor` of the animated cursor does NOT stop a running
  animation: the binding-by-host-handle survives resource-table
  removal, and X11 semantics keep an in-use cursor alive (see Frame
  lifetime in the data model section). The animation ends when the
  effective cursor changes, like any other cursor.

### Frame tick

- `next_wakeup()` (`kms/v2/backend.rs:8217`) additionally chains
  `ActiveCursorAnim.next_frame` — but only when outputs are active
  (see gating below). This bounds the core loop's poll timeout
  (`run.rs:409-437`) so the loop wakes by the deadline.
- **Concrete tick site:** there is no generic "deadline fired"
  backend callback after poll; the only unconditional per-iteration
  backend hook is `maybe_composite()` (`run.rs:741`). The tick runs
  at the top of `maybe_composite()`, *after* its existing
  `scanout_allowed()` / `kms_outputs_active` gates — which gives the
  DPMS/VT gating below for free: if `now >= next_frame`, advance
  `frame = (frame + 1) % n` and display the new frame (below), then
  re-arm `next_frame = now + delay[frame]` (relative re-arm,
  self-heals after a stall; if multiple deadlines were missed while
  stalled, advance once — do not fast-forward through missed
  frames).
- **Displaying a frame** mirrors the tail of
  `refresh_effective_cursor()` **including the sample-view readiness
  guard at `backend.rs:1139-1148`** (Vk-less fixtures build records
  without sprite allocations — the headless unit tests below walk
  straight into this path), for the new frame's record + pixmap:
  update `cursor_records[handle]` and `cursor_pixmaps[handle]` to
  the frame's entries (keeps XFixes and any other reader
  frame-correct), then — readiness permitting — scene
  `register_cursor(CursorEntry { id: frame.pixmap, .. })` (SW path
  samples the pixmap — updating the record alone would leave SW
  compositing on frame 0) and `queue_steady_state_cursor_upload()`
  when in Hw/Mixed plane mode and ≤64×64. Factor this shared tail
  into a helper rather than duplicating it.
- **Version/serial:** XFixes' `ActiveCursorImage.serial` is
  contractually monotonic (`trait_def.rs:71-74`), and the upload
  path dedups on version. Reusing each frame record's own version
  would repeat `v1,v2,v3,v1,…` on wraparound — backwards. Each tick
  therefore mints a **fresh** version from the same monotonic
  counter that stamps new `CursorRecord`s, and passes it as
  `record_version` / upload version / XFixes serial. The frame
  records themselves stay immutable. Mechanically: the tick inserts
  `cursor_records[handle] = Arc::new(CursorRecord { version: fresh,
  ..frame.record's fields })` — a ≤16 KiB clone per tick for HW-size
  cursors — so every reader picks up the minted version with no new
  branches.

### DPMS / VT gating

Frame ticks and uploads are gated on `kms_outputs_active` and
`scanout_allowed()` exactly like `maybe_composite` (EINVAL-storm
lesson, 2026-05-30): while outputs are off or VT is switched away,
`next_wakeup()` does not report the anim deadline (no wakeups burned
on an invisible cursor) and no uploads happen — `next_frame` is
simply left in place. On wake/VT-return the stale deadline is in the
past, so the first loop iteration advances **one** frame immediately
(the no-fast-forward rule above) and re-arms from `now`; normal
cadence resumes from there. No reset hook needed.

### XFixes GetCursorImage

Must return the *current frame* with a monotonic serial (Xorg
behavior). The KMS paths that serve cursor images out of
`cursor_records` (`kms/v2/backend.rs:4167,14030,14390`) read the
animated cursor's `cursor_records` entry — which the tick replaces
with the current frame's bytes under a freshly-minted version (see
Frame tick). No XFixes-specific changes needed.

### Error handling

- Handler validation errors: `BadCursor` for unknown sub-cursor
  (unchanged); new `BadValue` for an empty frame list, `BadLength`
  for a non-multiple-of-8 list, `BadMatch` for nested animated
  cursors (see Request handler change).
- `create_anim_cursor` backend failure (sub-cursor record/pixmap
  missing — "can't happen" after handler validation): propagate as
  the handler's existing backend-error path; no partial state
  (insert into maps only after all lookups succeed).

## Testing

1. **Unit (KMS backend, no display):** existing test harness at
   `kms/v2/backend.rs:16476+` style —
   - create 3 cursors, create anim cursor → `cursor_records[anim]` /
     `cursor_pixmaps[anim]` are frame 0; `next_wakeup()` includes
     the deadline.
   - simulate tick past deadline → frame advances, wraps mod n,
     `cursor_records[anim]` and `cursor_pixmaps[anim]` follow.
   - **serial monotonicity across wraparound:** versions strictly
     increase over ≥ 2n ticks (the regression Codex flagged: naive
     Arc-swapping repeats `v1,v2,v3,v1`).
   - **SW path:** the `CursorEntry.id` registered with the scene
     changes to each frame's pixmap across ticks (not just the
     record bytes).
   - effective cursor switches away → anim cleared, `next_wakeup()`
     no longer reports it; switch back → restarts at frame 0;
     re-resolve to same cursor → frame index preserved.
   - client FreeCursor of the anim cursor while it is effective →
     animation keeps running (keep-alive-while-referenced); next
     effective-cursor change ends it cleanly.
   - sub-cursors freed after creation → frames still cycle
     (backend-side records survive; status-quo no-op free).
   - nested-anim `BadMatch` also on a `None`-returning (fallback)
     backend — the `anim` flag is core-level.
   - delay 0 clamped to 16 ms.
   - handler errors: empty list → `BadValue`; odd pair bytes →
     `BadLength`; nested anim cursor → `BadMatch`.
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
- Three maps keyed by the same host handle (`cursor_records`,
  `cursor_pixmaps`, `anim_cursor_records`) must stay in sync on
  create and tick (same discipline as the existing sibling-map
  comment at `backend.rs:277`; nothing is removed on free — see
  Frame lifetime).
- The per-tick record clone (fresh-version mechanism) allocates;
  bounded at cursor size (≤16 KiB HW, larger for SW cursors) per
  frame at 30–100 ms cadence — acceptable, but keep it out of any
  hotter path.
