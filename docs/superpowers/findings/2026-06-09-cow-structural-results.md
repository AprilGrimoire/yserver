# COW Structural Redesign — implementation results

**Date:** 2026-06-09
**Branch:** `feat/cow-structural` (off `master`)
**Spec:** `docs/superpowers/specs/2026-06-08-cow-structural-design.md`
**Plan:** `docs/superpowers/plans/2026-06-08-cow-structural.md`

## What shipped

Replaced the split COW model (resources record + drawable-store storage +
scene-only special append) with the Xorg-shaped one: the Composite Overlay
Window is a real child of root in both `resources.rs` and the v2 backend
(`windows_v2` + `top_level_order`); it emits via the normal scene walk;
Manual-redirected windows never emit directly to scanout; root hit-testing
treats the COW as a transparent container.

Six phases, executed subagent-driven with per-task spec + code-quality
review:

| Phase | Outcome |
|---|---|
| 1 — `cow_aware_top_index` helper; all `root.children` mutators funnel through it | ✅ |
| 2 — COW lifecycle (materialize/teardown), scene rewrite, input transparency | ✅ |
| 3 — Manual-redirected windows skip emit unconditionally | ✅ |
| 4 — drift panics + Xorg-faithful reparent/redirect | ✅ (3 corrections, below) |
| 5 — delete obsolete `cow_authoritative` machinery | ✅ |
| 6 — integration test + validation + HW matrix | ✅ |

## Course-corrections (all toward strict Xorg fidelity)

1. **Hit-testing (invariant 7).** Dropped the planned COW-special "always
   descend" branch. Once the COW is a real root child, the existing
   `window_input_contains` gate in `hit_test_child` gives Xorg-faithful
   `miSpriteTrace` semantics for free (empty input shape ⇒ pass-through;
   non-empty ⇒ descend). No COW-special code.

2. **Reparent/Redirect (Phase 4.3/4.4).** The original plan invented
   `BadMatch` errors that Xorg does not raise. Verified against the local
   Xorg tree: `compRedirectWindow` (compalloc.c:145-147) returns `Success`
   no-op for the COW; reparent has no COW-specific rejection (only the
   generic cycle rule, already enforced in `ResourceTable::reparent_window`).
   Reset the two wrong commits, re-implemented Xorg-faithful.

3. **Reparent-to-root panic (Phase 4.1).** Code review caught a
   server-crash regression: the drift-panic fired on every
   `ReparentWindow(child → root)` because the root sentinel is
   `core.window_id` (==1), not `0`. Fixed with a regression test.

4. **COW redirected by `RedirectSubwindows(root)` (Phase 4.5).** Found on
   HW (see below). Making the COW a real root child exposed it to a
   compositor's `CompositeRedirectSubwindows(root, Manual)`, which redirected
   the COW itself → `scene_participating=false` → Phase 3 Manual-skip dropped
   the entire composited desktop. Fixed by guarding the single
   redirect-activation chokepoint (`activate_redirect_backing_for`),
   mirroring Xorg's `compCheckRedirect` (compwindow.c:156-170). This is the
   load-bearing lesson: **the COW must never be *actually* redirected via any
   path, not just explicit `RedirectWindow`.**

## Validation

**Unit / integration (sandbox):**
- `yserver-core --lib`: 660 passed / 48 failed — the 48 are **byte-identical
  to master** (pre-existing XI/property failures, zero regressions; +16 new
  passing tests across the branch).
- `yserver --lib`: 441 passed / 0 failed.
- Headless integration test
  (`compositor_stage_under_cow_emits_via_recursion_and_manual_siblings_skip`)
  runs (not `#[ignore]`d) and proves: Manual-redirected sibling emits 0
  draws; stage (COW child) emits 1 draw with `alpha_passthrough`; COW emits 1
  draw with `alpha_passthrough`; correct ordering.
- `cargo +nightly fmt` clean; `cargo clippy` (plain) — no new warnings
  (18 pre-existing yserver-core warnings, identical count to master).
- debug + release builds green.

**Hardware (bee):**
| Session | Result |
|---|---|
| cinnamon-mutter (golden case) | ✅ desktop renders; keyring/seahorse/logout dialogs on top; clicks land correctly |
| mate | ✅ clean, shadows and all (non-regression) |
| xfce / xfwm4 | ✅ works |
| e16 (no compositor) | ✅ works |

**rendercheck (vng):** fill/dcoords/scoords/mcoords/tscoords/tmcoords/blend
all OK; the default 600s budget times out mid-suite at `composite` (vng
slowness, not a regression) — needs ~900-1200s. RENDER's compositing path is
orthogonal to the COW/Composite-extension changes; no regression expected.

**XTS:** orthogonal to the COW/redirect/scene-walk changes; no regression
expected (release-gate run optional).

## The HW root-cause, in one trace

```
#386 COMPOSITE::RedirectSubwindows(0x100, mode=Manual)
allocate_redirected_backing W=0x103: fresh B=0x400028 (2560x1440, depth=24)
set_redirected_target window=DrawableId(22) old=None new=Some(DrawableId(35))
scene_walk xid=0x103: SKIP reason=manual_redirect_unconditional_skip scene_participating=false
```
The COW (`0x103`) was emitting (`WILL_EMIT scene_participating=true`) until
mutter's `RedirectSubwindows(root, Manual)` redirected it. Fixed by Phase 4.5.

## Known follow-up (not a blocker)

The COW sits mid-`top_level_order` rather than topmost — the backend
projection isn't COW-aware the way Phase 1 made `resources.root.children`
COW-aware. Benign for mutter-class compositors (the entries above the COW are
Manual-redirected and skip emit, so the COW is still the last *emitted*
top-level — confirmed: cinnamon renders correctly). Could surface as a
stacking glitch under a non-Manual-redirect arrangement. Track separately;
fix with evidence if it bites.
