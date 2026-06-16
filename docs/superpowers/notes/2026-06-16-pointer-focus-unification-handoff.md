# Hand-off: unify pointer + focus hit-testing (kill the dual source)

Date: 2026-06-16. Status: **root cause found, fix not started.** This doc is
self-contained — a fresh session should be able to continue from here.

## Goal
Cinnamon sloppy mouse-focus on yserver is broken. We need pointer-event
delivery (clicks/crossings) and keyboard focus to resolve the
window-under-pointer through **one** authority, like Xorg — so the focused
window and the clicked window can never disagree.

## Symptoms (HW: silence, GhostBSD/Linux + cinnamon + wezterm/gkrellm)
- **Original (pre-#34 / on the revert):** sloppy-focus flaky — hovering a
  window often fails to focus it (worst on wezterm). Clicks work; focus is
  just unreliable.
- **With #34 merged (current master):** worse — "one window has focus, the
  window below gets the click"; gkrellm buttons don't respond. Clicks resolve
  to a different window than focus.
- Both are the **same bug** (one hit-test wrong vs another), different
  symptom. There is no good commit to sit on — must fix properly.

## Root cause (confirmed from logs + code, 2026-06-16)
yserver keeps the window **input-shape in TWO stores**, each consulted by a
*different* hit-test:
- `ServerState.shape_windows` → resource-space hit-test
  `server.rs::root_pointer_target_at` (focus/keys, and pre-#34 everything).
  Written by the protocol XFixes/SHAPE path (`process_request.rs:3416`,
  `nested.rs:918`).
- `KmsCore.shape_input` (`kms/core.rs:1574`) → backend hit-test
  `backend.rs::window_under_cursor` (clicks, post-#34 via host_xid). Written
  at `backend.rs:15039`.

Two stores written by different paths → they diverge, worst on the **Composite
Overlay Window** (`0x103`) + cinnamon's stage (`0xb00011`), whose input region
cinnamon **toggles ~20×** (`SetWindowShapeRegion kind=Input region=None` vs a
real region) for dynamic passthrough. When the two stores disagree, clicks
land in the COW/stage subtree (trace: `target=0xb00011 core_targets=[]`)
instead of the app. There is ALSO a historical None-vs-empty-region
interpretation bug (memory: "inverted empty→opaque"): `region=None` means
*remove shape → opaque*; an *empty* region means *click-through*. Verify both
stores agree on that.

So "resources lags" is really "a second shape store disagrees."

## Xorg model = the target (from ../xserver, studied 2026-06-16)
ONE window tree, ONE sprite trace, used for pointer AND keyboard:
- `mi/miwindow.c:749 miSpriteTrace`: from root, descend picking the TOPMOST
  (`firstChild`-first) mapped child whose geometry+border contains (x,y) AND
  passes bounding shape (`PointInBorderSize`) AND input shape
  (`RegionContainsPoint(wInputShape, x-wx, y-wy)`); record path in
  `spriteTrace[]`. `!wInputShape` ⇒ treated as hit (opaque); empty input
  shape ⇒ skipped (click-through). InputOnly windows ARE hittable.
- `dix/events.c:3152 CheckMotion`: recompute sprite on motion/button AND on
  `WindowsRestructured` (map/unmap/restack under a stationary pointer — the
  map-time recompute yserver lacks). Emit Enter/Leave when sprite changes.
- Pointer delivery walks `spriteTrace`. Key delivery
  (`dix/events.c:4215 DeliverFocusedEvent`): PointerRoot or sprite-in-focus-
  subtree ⇒ keys to the SAME sprite; else to the focus window.

## Fix plan
1. **One input-shape store.** Collapse `shape_windows` + `KmsCore.shape_input`
   into a single source updated on every `SetWindowShapeRegion(kind=Input)`
   (incl. the COW), with correct None(opaque)/empty(click-through) semantics.
2. **One hit-test** — a miSpriteTrace-faithful walk over the `resources` tree
   honoring that store — used for clicks, crossings, motion, PointerRoot
   keys, and XIQueryPointer.
3. Host-first (#34) becomes unnecessary; keep it reverted.
4. (Separate, pre-existing) recompute the sprite on restack/map under a
   stationary pointer — Xorg's WindowsRestructured path. See
   `project_yserver_cursor_focus_bugs` (map-time crossing).

## DO NOT repeat (failed attempts)
- **#34 host-first routing** — added a 2nd authority for pointer delivery,
  left focus/keys on resource-space → focus≠click. Reverted (branch
  `revert-34-host-first-hittest`, 73d9317).
- **`ServerState.pointer_sprite` cache** (branch `fix/unified-pointer-sprite`,
  now reset to master) — had `deepest_window_at_pointer` return a cached
  host-first sprite. Built/tested green but on HW **could not click anything**
  (`core_targets=[]`, `xi1_route hit=None`); mechanism never understood.
  Lesson: this input path has consumers that aren't obvious from grep — do
  NOT one-shot from theory.

## Process discipline (this bug has burned several confident wrong fixes)
- Evidence first. Reproduce on HW (silence, cinnamon + wezterm/gkrellm) with
  `RUST_LOG=...pointer=trace,...pointer_fanout=debug` and capture a yserver
  xtrace; the matching **Xorg xtrace of the same action is the reference**
  (`feedback_visual_diff_is_our_bug`).
- Find the exact consumer/divergence before editing. Verify None-vs-empty
  region handling in BOTH stores as the immediate next read.
- Don't add a second source of truth to compensate for an inaccurate first
  one (the recurring anti-pattern here).

## Git state
- `master` (e186004a) — has #34, IS the broken focus≠click state.
- `revert-34-host-first-hittest` (73d9317, pushed) — reverts #34; build/test
  green (747 core + 497 yserver). **Recommended base** + the stopgap to land
  so master stops shipping #34. PR not yet opened.
- `fix/unified-pointer-sprite` — reset to master (failed experiment, ignore).
- `fix/copygc-arc-mode-value-mask` (5e672b6, pushed) — unrelated, gkrellm
  BadValue fix, awaiting decision.

## Key files
- `crates/yserver-core/src/server.rs` — `root_pointer_target_at`,
  `shape_windows`, `pointer_root`.
- `crates/yserver-core/src/core_loop/pointer_fanout.rs` —
  `natural_pointer_target` / `host_pointer_target` / fanout entry (~249).
- `crates/yserver-core/src/core_loop/key_fanout.rs` —
  `deepest_window_at_pointer` (focus/key sprite resolver).
- `crates/yserver/src/kms/v2/backend.rs` — `window_under_cursor`,
  SetWindowShapeRegion handler (~15039).
- `crates/yserver/src/kms/core.rs:1574` — `shape_input`.
- Reference impl: `../xserver/mi/miwindow.c` (miSpriteTrace),
  `../xserver/dix/events.c` (CheckMotion, DeliverFocusedEvent).
- Full background: memory `project_sloppy_focus_dual_hittest`.
