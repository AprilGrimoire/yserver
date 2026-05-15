# COMPOSITE Manual-mode redirect — backing-as-source path

Branch: `render-convolution-filter` (current). Builds on the
2026-05-14 fix that decoupled redirect registration from backing
activation (`d8ddb3f`, kept under `#[allow(dead_code)]`).

## Motivation

Visible symptom: thin opaque dark bars to the right and below
xfce pop-up menus (any GTK Client-Side-Decoration popup with
alpha-shadow margins). Same gap affects every Manual-mode
compositor: xfwm4, picom's xrender backend, xcompmgr, compton.

Fault chain (full version in `docs/known-issues.md` under
"Compositor shadow margins render as opaque bars"):

1. xfwm4 issues `RedirectSubwindows update=Manual(0x01)` on root.
   Accepted today, redirect record is registered.
2. The 2026-05-14 decoupling skipped `activate_redirect_backing_for`,
   so `host_window_to_backing` stays empty for every redirected
   window.
3. xfwm4 issues `NameWindowPixmap` for the menu.
4. `KmsBackend::name_window_pixmap` looks up
   `host_window_to_backing`, finds nothing → `NotFound` → wire
   **BadAlloc** on `Composite-Request(144, 6)`.
5. xfwm4 falls back to `CreatePicture` on the live window. The
   live window only carries the menu's visible RGB; the GTK CSD
   shadow-alpha lives in the offscreen backing we never
   allocated, so xfwm4 emits opaque shadow-colour pixels where
   the alpha gradient should fade.

The fix is to honour step 2 properly: allocate the backing on
Manual redirect, route paint into it, and arrange the scanout
pass so the redirected window's stale own-mirror doesn't stack
on top of what the compositor paints to the root.

## Why this is safe — why MATE broke before, and why Manual-only avoids it

`92a2a83` (reverted at `3751c11`) activated backings on **both**
Automatic and Manual. That diverted paint to the backing pixmap
for every redirected window. For Manual that's correct (the
spec says so); for Automatic the server is supposed to keep
drawing the window to the screen and merely mirror into the
backing for the compositor. With no Auto-mode mirror-AND-screen
path implemented, Automatic-mode windows went invisible →
"black screen on MATE".

This plan only activates backings for `Manual`. Automatic-mode
clients (mate-panel's notification-area-applet, anything else
that requested Auto historically) keep the current behaviour:
redirect record registered, backing not allocated,
NameWindowPixmap returns NotFound→BadAlloc. Most Auto-mode
clients don't actually call NameWindowPixmap — they request
Auto to track damage on a subtree, not to read its contents.
If a future client needs the Auto-mode backing-AND-screen path,
that's a separate piece of work.

## Scope

In scope:

- `activate_redirect_backing_for` becomes live, gated on
  `mode == Manual`.
- Three activation sites:
  - REDIRECT_WINDOW (single window): activate the named window
    if `mode == Manual`.
  - REDIRECT_SUBWINDOWS (parent + children): activate every
    current direct child of the named parent if `mode == Manual`.
  - CreateWindow new-child hook: if the new window's parent
    has an active Manual REDIRECT_SUBWINDOWS record, activate
    the freshly-created child.
- **Mode-transition / re-redirect** (codex P1.1): the same
  client may overwrite its own record with a different mode
  (`composite_redirects.insert` already allows same-owner
  overwrite at `process_request.rs:2647`). When the new mode
  differs from the old:
  - `Manual → Automatic`: tear the backing(s) down (call the
    same per-window teardown the unredirect path uses) and
    pull the window(s) out of the scanout-skip set.
  - `Automatic → Manual`: activate backing(s) and add to the
    scanout-skip set.
  - Same-mode re-redirect: idempotent (no-op beyond the
    existing record overwrite).
- Scanout-walk policy: skip pushing the own-mirror quad for a
  window flagged as redirected-Manual. The window's visual
  content is whatever the compositor paints to the root (or
  another window) via RENDER Composite from the named pixmap.
  Recursion into descendants continues — see "Subtree scoping"
  below.
- Backend skip-set API (codex P2): expose a small explicit
  trait method
  `set_window_scanout_skipped(host_window, skip: bool)` rather
  than auto-toggling inside `allocate_redirected_backing` /
  `release_redirected_backing`. The auto-toggle approach
  conflicts with `rotate_redirected_backing_on_resize`
  (`process_request.rs:580`), which allocates a new backing
  then releases the old one — releasing the old must not
  clear the flag for the still-redirected window. Explicit
  call sites at activate / tear-down / mode-transition keep
  the lifecycle obvious.
- Teardown audit, all five exit paths:
  - UNREDIRECT_WINDOW — single-window teardown via
    `teardown_redirect_for_window`. Existing.
  - UNREDIRECT_SUBWINDOWS — currently does NOT walk children
    for teardown. Add a children walk.
  - Client disconnect — `process_disconnect` (codex P1.3):
    currently iterates the disconnecting client's redirect
    records and calls `teardown_redirect_for_window` keyed on
    each record's `window`. For root+REDIRECT_SUBWINDOWS the
    record's window IS root, which has no `redirected_backing`
    of its own — the *children* hold backings, so the call is
    a no-op and backings leak. Fix: when the record is a
    subwindows redirect, walk
    `state.resources.children(record.window)` and tear each
    down. Add a `subwindows` field to the
    `owned_redirects` collection so the walker knows.
  - DestroyWindow — currently inlines backing collection +
    `backend.release_redirected_backing` rather than using the
    shared helper (codex P1.4,
    `process_request.rs:740`). Two options: (a) replace the
    inline block with calls to `teardown_redirect_for_window`
    which already handles the resource-side cleanup; (b) keep
    the inline block but add an explicit
    `backend.set_window_scanout_skipped(..., false)` call per
    destroyed window. Prefer (a) — single helper means the
    scanout-skip cleanup lives in one place.
  - Mode-transition Manual→Automatic — already covered above.
- Resize: `rotate_redirected_backing_on_resize` already exists;
  ensure it's wired into the resize handler for Manual-redirected
  windows. The skip-set entry must survive the rotation (old
  backing is released, new backing is allocated, the window
  stays redirected the whole time → no skip-set toggle).
- Damage on backings (codex P1.2 — now in scope, was T5):
  - xfwm4 typically issues `XDamageCreate` on the window XID
    (not on the named pixmap) — confirm during T4 by capturing
    an actual xfwm4 trace. If that holds, our existing damage
    path is sufficient: PutImage (and every other paint route)
    damages `request.drawable` (the window), the
    `damage_objects` table fires on `damage.drawable ==
    drawable`, xfwm4 gets the notification and re-composites
    from the named pixmap whose content was updated via the
    same paint pipeline (now routed through the backing).
  - **If the trace shows xfwm4 damages the named pixmap**:
    that's a real gap and the fix lands in this plan, not a
    follow-up. Shape: when accumulating damage on a redirected
    window, also fire damage on any live `NameWindowPixmap`
    aliases of its backing. `Window.composite_named_pixmaps`
    already tracks the alias list per window. **The alias's
    `client_pixmap: ResourceId` is the correct match key — not
    `host_pixmap`**: `DamageObject.drawable` is a `ResourceId`
    (the client-side XID the client passed to `DamageCreate`),
    so the alias the client sees is `client_pixmap`. Firing on
    `host_pixmap` would miss every real DamageObject.

Subtree scoping (codex Open Question, kept in plan):

- The scanout-skip applies only to the redirected window's own
  mirror; recursion into children continues. For single-top-
  level CSD windows (xfce menu, tooltip, GTK CSD popup) there
  are no input-output children to worry about — the popup IS
  its own draw surface.
- For xfwm4-frame-wrapping a client toplevel, the frame is a
  direct child of root and gets redirected. The client window
  is a child of frame — not directly redirected by
  RedirectSubwindows(root). Its mirror would still draw to
  scanout. That's not strictly correct under X11 Composite
  semantics (where redirect captures the full subtree's paint
  into one backing), but yserver's per-window mirror model
  already diverges from that semantic. **Out of scope for
  this plan**: the immediate goal is fixing the xfce menu
  shadow (single-window case). Full subtree semantics for
  xfwm4-frame-wrapping is a separate piece of work; file a
  follow-up in `known-issues.md` once the menu case is
  validated working.

Out of scope:

- Automatic-mode backing path (would need paint-to-both
  semantics — a separate piece of work).
- Full subtree paint capture under RedirectSubwindows (see
  subtree scoping note above).
- **Overlapping Manual redirects on the same window.** A child
  can in principle be covered by BOTH `RedirectWindow(child,
  Manual)` AND `RedirectSubwindows(parent, Manual)`
  simultaneously. Properly handling teardown in that case
  requires "only drop the child's backing when no other active
  Manual record still covers it" refcounting. X11's compositor
  election (the COMPOSITE manager selection) means only one
  client is supposed to redirect any subtree at a time, so
  this is a corner case in practice. **Documented as
  unsupported**: if a future bug surfaces from overlap, the
  fix is a teardown-time check "is this window still covered
  by another active Manual record?" gating the backing drop +
  skip-set clear. Until then, the teardown walks in T3
  unconditionally drop the per-child state.
- xfwm4 / picom polish beyond getting the shadow to render
  correctly. If a separate compositor bug surfaces, file it.
- The Damage / XFixes gap that blocked picom in the convolution
  smoke (filed separately; convolution-pipeline work paused
  pending a real consumer).

## Task breakdown

### T1 — backend skip-set + trait method

`crates/yserver/src/kms/backend.rs`:
- New field `KmsBackend.windows_redirected_manual: HashSet<u32>`
  (host-window-XID keyed).
- `walk_subtree_into_draws`: skip the own-mirror push when
  `windows_redirected_manual.contains(&window_id)`. Recursion
  into children continues as today.

`crates/yserver-core/src/backend/trait_def.rs`:
- New trait method
  `fn set_window_scanout_skipped(&mut self, origin: Option<OriginContext>,
  host_window: WindowHandle, skip: bool)`.
- `KmsBackend` impl inserts/removes from the new HashSet.
- `RecordingBackend` impl records the call for test assertions.
- `HostX11Backend` impl is a no-op (the host X server is the
  scanout in that path, not the compositor we're routing
  around).

Tests:
- Backend-level: insert a window into the skipped set, build a
  composite scene, assert no draw for that window's mirror is
  emitted; ensure descendant windows still appear.

### T2 — flip the activation gates (Manual-only)

`crates/yserver-core/src/core_loop/process_request.rs`:

- Drop the `#[allow(dead_code)]` on `activate_redirect_backing_for`
  and have it call `backend.set_window_scanout_skipped(..., true)`
  after the successful `allocate_redirected_backing`.
- Restore `parent_has_subwindows_redirect` (deleted in `d8ddb3f`),
  tightened to require `Manual` mode — rename to
  `parent_has_manual_subwindows_redirect`. Re-wire the
  CreateWindow new-child hook to call
  `activate_redirect_backing_for` when this returns true.
- REDIRECT_WINDOW / REDIRECT_SUBWINDOWS handler, after
  inserting the redirect record:
  - **Mode-transition gating** (codex P1.1): inspect the
    record's prior mode (read via `composite_redirects.get`
    *before* the insert). Four cases:
    - Prior absent + new Automatic: register record, do
      nothing else.
    - Prior absent + new Manual: register record, activate
      backing(s).
    - Prior Manual + new Automatic: register record (new mode),
      walk affected windows and tear backings down (via
      `teardown_redirect_for_window`).
    - Prior Automatic + new Manual: register record (new mode),
      activate backing(s).
  - Idempotent same-mode re-redirect requires no state change.

Tests at this layer:
- `redirect_window_manual_activates_backing` — REDIRECT_WINDOW
  with mode=Manual sets `Window.redirected_backing` and calls
  `set_window_scanout_skipped(true)` on the backend.
- `redirect_window_automatic_skips_backing` — mode=Automatic
  leaves `redirected_backing = None` and does NOT skip-scanout
  (regression guard against 92a2a83).
- `redirect_subwindows_manual_walks_children` — children of the
  named parent each get a backing.
- `create_window_under_manual_subwindows_inherits_backing` —
  new child gets one.
- `mode_transition_manual_to_automatic_tears_backing` — change
  mode in place; backing torn down, scanout-skip flag cleared.
- `mode_transition_automatic_to_manual_activates_backing` —
  inverse.

### T3 — teardown audit on all exit paths

- UNREDIRECT_SUBWINDOWS handler: after removing the record,
  walk `state.resources.children(parent)` and call
  `teardown_redirect_for_window` for each. (Today only
  UNREDIRECT_WINDOW does the per-window teardown.)
- `teardown_redirect_for_window`: in addition to
  `backend.release_redirected_backing`, call
  `backend.set_window_scanout_skipped(host_window, false)` so
  the skip-set drops the entry. The helper currently only
  reads `redirected_backing` via `.take()`; to feed the
  scanout-skip API a `WindowHandle`, snapshot both
  `(host_xid, redirected_backing)` from the window in one
  borrow scope **before** the `take()`, then issue the two
  backend calls outside the borrow. Skip the
  `set_window_scanout_skipped` call when `host_xid` is `None`
  (window never got a host XID — backing couldn't have been
  activated either).
- `process_disconnect` (codex P1.3): change the
  `owned_redirects` collection to capture
  `(window, subwindows)` tuples instead of just `window`.
  When the record is a subwindows redirect, walk
  `state.resources.children(window)` and call
  `teardown_redirect_for_window` per child. Single-window
  records keep the existing per-window call.
- DestroyWindow (codex P1.4): replace the inline backing
  collection + `backend.release_redirected_backing` loop at
  `process_request.rs:740` with calls to
  `teardown_redirect_for_window` per destroyed window. That
  routes scanout-skip cleanup through the same shared helper.

Tests:
- `unredirect_subwindows_tears_down_all_children` — children's
  `redirected_backing` cleared post-unredirect.
- `client_disconnect_clears_manual_subwindows_backings` — root+
  REDIRECT_SUBWINDOWS owner dies, all per-child backings are
  released, every child cleared from the backend skip-set.
- `destroy_window_clears_skip_set_entry` — destroy a
  Manual-redirected window, assert the backend skip-set no
  longer contains its host XID.

### T4 — resize wiring + Damage spot-check

Resize:
- Confirm `rotate_redirected_backing_on_resize` is called from
  the existing ConfigureWindow resize handler for
  Manual-redirected windows. If it isn't, wire it.
- Assert that the resize rotation does NOT toggle the
  scanout-skip flag — the window stays redirected the whole
  time.

Damage (validation, may widen scope):
- Capture x11trace under xfwm4 + an xfce menu open/close
  cycle. Record the DamageCreate target xid: window xid →
  existing path works; pixmap xid → widen this task to fire
  damage on `Window.composite_named_pixmaps` aliases when
  accumulating damage on the redirected window.
- Damage-path widening (if needed): in
  `accumulate_damage`, after the existing window-keyed fire,
  iterate `window.composite_named_pixmaps` and fire on each
  alias' `client_pixmap` so xfwm4's `XDamageCreate` on the
  named pixmap also notifies. (`DamageObject.drawable` is a
  `ResourceId` — match the client-side XID, not
  `host_pixmap` which is a backend `PixmapHandle`.)

Tests:
- `resize_redirected_manual_window_rotates_backing` — pre-resize
  alias survives via the registry refcount; post-resize
  `Window.redirected_backing` points to the new backing at the
  new dimensions; skip-set membership unchanged.
- (If Damage widening lands) `damage_on_redirected_window_fires_on_named_pixmap_alias`.

### T5 — full test + lint pass

- `cargo test --workspace`
- `just rendercheck-yserver`
- `cargo clippy --workspace --lib --tests` (regular clippy
  only, per AGENTS.md:11 — pedantic is not required in this
  repo). Fix any new warnings introduced by this branch.
- `cargo +nightly fmt`

### T6 — hardware smoke (user-owned)

- Fuji (Intel): `just yserver-xfce-hw` → open xfce4-panel apps
  menu / a right-click menu, observe the shadow gradient
  renders smooth (not bars). Confirm panel & desktop still
  render normally (regression guard for MATE-class breakage).
  Also exercise mode-transition: log in, observe redirect
  record updates from any compositor activity, confirm no
  visual regression.
- Bee (RDNA2): same recipe; ensure no GPU faults under the
  new paint-to-backing route.
- Picom: `just yserver-picom-hw` — known to stop after one frame
  due to a separate Damage/XFixes gap. If picom now makes
  visible progress, file as a bonus; if not, that's still
  diagnosed-elsewhere and not a blocker for this plan.

## Risks + rollback

- **Manual-redirect window invisibility** — by design, the
  redirected window's mirror is no longer in the scanout walk.
  Visible content depends on the compositor doing its job
  (RENDER Composite from named pixmap → root). xfwm4 does;
  picom does. A buggy/missing compositor produces an invisible
  window. That matches real Xorg semantics under Manual mode.
- **Owner-disconnect path** — must drop the skip-set entries
  the disconnecting client's records covered (codex P1.3),
  else windows stay invisible after the compositor dies.
- **Skip-set stale entries** — if DestroyWindow doesn't route
  through the shared teardown (codex P1.4), dead/reused host
  XIDs stay in the skip set and the next window allocated at
  that XID is invisible. T3's plan eliminates this.
- **Mode-transition path** — if Manual→Automatic doesn't clear
  the skip-set entry, an Automatic-redirected window stays
  invisible.
- **Resize race** — `rotate_redirected_backing_on_resize`
  releases the old backing inside a window still marked
  redirected. The skip-set toggle MUST be at the
  activate / teardown boundaries, not inside
  allocate/release, or resize accidentally clears the flag.
  Explicit trait method (T1) avoids this.
- **`renderer_failed` paths** — the new
  `set_window_scanout_skipped` and cleanup paths must not
  panic under the strict-flush failure regime (Phase 3B
  salvage discipline).

Rollback strategy: every task lands as its own commit. If
smoke regresses, revert that task's commit; the prior T# task
lands stand-alone. T1 (skip-set + trait method) is the only
task that adds new infrastructure; T2-T4 are pure call-site
wiring that can be reverted independently.

## Source-of-truth pointers

- 2026-05-14 decoupling that left the gap: commit `d8ddb3f`.
- Reverted experiment that broke MATE: `92a2a83` → `3751c11`.
- Backend infrastructure (already in place):
  `crates/yserver/src/kms/backend.rs` —
  `allocate_redirected_backing`, `name_window_pixmap`,
  `release_redirected_backing`, `host_window_to_backing`,
  `alias_registry`.
- Core-side dormant helpers: `activate_redirect_backing_for`,
  `rotate_redirected_backing_on_resize` in
  `crates/yserver-core/src/core_loop/process_request.rs`.
- Window-state field: `Window.redirected_backing` in
  `crates/yserver-core/src/resources.rs`.
- Drawable routing: `host_drawable_target` already consults
  `redirected_backing` to redirect paint.
- Teardown helper: `teardown_redirect_for_window` in
  `crates/yserver-core/src/core_loop/process_disconnect.rs`.
- Existing redirect-record overwrite policy:
  `process_request.rs:2647` (same-owner overwrite allowed).
- Disconnect teardown call site:
  `process_disconnect.rs:194-202`.
- DestroyWindow's inline backing-release loop:
  `process_request.rs:740-756`.
- Damage delivery match predicate:
  `damage_fanout.rs:60`.
