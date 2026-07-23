# Idle compositor free-run — per-output recompose + drain-consistency

**Branch:** `fix/idle-compositor-multioutput` (off `master` @4dfb8936)
**Regresses:** PR #30 (`2026-06-14-idle-compositor-cursor-damage.md`, @2d1bdc61) — idle-stop is back to 60 fps.
**Related memory:** `project_idle_compositor_redraw_loop`.

## ⚠️ STATUS 2026-07-20 — DO NOT MERGE: cut 1 regresses the 01656908 submenu fix (handoff for bee)

Branch HEAD `7d11f439` = cut1 `9c7962aa` + cut2a `f1af8dab` + cut2b `7f912519` + a **throwaway
diag** `7d11f439` (`empty-damage-diag` logging; REMOVE before any real merge). The idle fix is
HW-confirmed good (Peter's GTX 1060: idle 16%→<1%; silence dual-mon: iter/s 926→2–5) — **but it
re-introduces the "hover a submenu → doesn't appear until you nudge the mouse" bug** (the 4-day
Claude+codex fix, commit `01656908`). **This is a correctness regression of a hard-won fix — treat
it with the same care as the original: gate EVERY iteration on the submenu repro + keep codex in
the loop.**

**Repro matrix:** breaks on bee (RADV/RDNA2), eiger + air (asahi) — NOT silence (RADV/Polaris, no
`VK_EXT_image_drm_format_modifier`) nor Peter's 1060 (NVIDIA). ⇒ multi-BO / modifier-path timing
dependent. Bisect (eiger): `9c7962aa` regresses, `4dfb8936` (master) correct ⇒ **cut 1 is the
regressor**. cut 2b is NOT proven safe for the submenu either (it reduces `has_pending`, and the
submenu's own damage is empty, so its compose is triggered by something else — cut 2b could break
it independently).

**Root cause (from the `empty-damage-diag` on eiger):** at the failing tick,
`out0 draws=4 carry=false snapshots=[(1,0),(209,0),(210,0)]` — the submenu IS drawn, but every
captured snapshot has an EMPTY region (rects=0). So it's **drawn-but-clean**: damage already
drained, yet its content isn't on the freshly-cycled scanout BO, so it needs a force-compose
(Repaint::Full) to land. Master's guard `!built.snapshots.is_empty()` (presence) force-composes it →
shows. cut 1's `snapshots_carry_damage()` (non-empty region) SKIPS clean snapshots → stranded.

**Why reverting cut 1 alone fails (CONFIRMED on silence): 120 fps idle storm.** The presence-gate
force-compose keeps outputs perpetually mid-flip → `all_walked` never true → **cut 2b's reconcile is
starved** → off-screen culprits never flagged → `has_pending` never drops → the tick never stops.
So cut 1 and cut 2b are interdependent.

**FUNDAMENTAL TENSION:** the idle fix REDUCES composes (how it saves CPU); the submenu fix DEPENDS
on a compose firing to land a clean window; they share the exact same trigger path.

**FIX DIRECTION (for the careful attempt — NOT yet implemented):** gate the compose on **scene-
content staleness of the acquired BO**, not on damage-region emptiness. Track a scene-content
generation that bumps only on a real change (presentation damage added / structure change); record
per-BO the gen it was last composed at; force-compose only when the acquired BO is behind the
current gen. This distinguishes idle-redundant recompose (all BOs current → skip → quiesce) from
submenu-needed (BO stale → compose) WITHOUT the damage-region heuristic that strands clean windows,
and without reducing `has_pending`. (Alt considered: revert cut 1 + de-starve cut 2b's reconcile via
per-output cached last-drawn-set unioned every tick — but cut 2b submenu-safety still unproven.)

**Confirmed-independent / safe:** cut 2a (auto-repeat, `f1af8dab`) is clean. Drag jank (op137 XI2
~15 ms blowing the 16.67 ms vblank budget; GNOME-Wayland ALSO janky on the 1060 ⇒ partly marginal
hardware) is a SEPARATE issue. Staging-pool `perf/nvidia-staging-buffer-pool` was a red herring
(children%/self% misread) — NOT the idle fix, do not merge for perf.

**Bee tonight:** repro the submenu with `just yserver-xfce-hw-telemetry` + hover a submenu;
`grep empty-damage-diag yserver-hw-xfce.log`. Implement the scene-content-staleness gate; verify (a)
submenu appears immediately on bee AND (b) idle stays quiet (`iter/s` low, no flip storm) on bee;
codex-review the compose-path change before committing; then remove the `7d11f439` diag.

## 2026-07-20 capture attempt (bee) — findings + why manual dump can't catch it

- **The regression is a LATENCY, not permanent stranding.** HW observation on bee: with the cursor
  held completely still, the stranded submenu **still appears eventually** on its own — it just lags.
  Consistent with the `empty-damage-diag` bee log (every submenu tick `carry=false snapshots all
  rects=0`) and the memory's "self-heals on the next incidental re-composite": cut 1 didn't kill the
  heal, it made incidental composes rare, so the heal is delayed by seconds instead of ~1 frame.
- **Manual external-signal capture is too slow to freeze the stranded frame.** Added a THROWAWAY
  `SIGWINCH → unconditional drawable+scanout dump` in `lib.rs` (SIGUSR1/SIGUSR2 are stolen by the
  armed direct-VT `VT_PROCESS`, and Ctrl-Alt-D is an input event that heals the menu). `kill -WINCH
  $(pgrep -x yserver, non-lightdm)` works and dumps `do_dump_scanout` (reads the front BO, no
  recompose) — but by the time a human says "now" over chat and the operator fires the signal, the
  menu has already self-healed. Every manual capture caught the *healed* frame.
- **NEXT (the real capture): auto-dump AT the strand instant.** Hook a one-shot, env-gated
  (`YSERVER_DUMP_ON_STRAND`) dump into the empty-damage skip site in `tick_one_output`
  (`scene.rs:1696`, right where cut 1 decides to skip a drawn-but-clean snapshot). Firing from inside
  the render loop removes the timing race entirely — it captures the exact tick the submenu is
  stranded. Rate-limit to once (AtomicBool) so it doesn't dump 60×/s. Then compare `scanout` (submenu
  absent) vs the submenu `win-*` backing (content present) to nail producer-alloc vs compose-gate.
- Throwaway `SIGWINCH` diag kept for now (harmless; remove with the `7d11f439` diag before any merge).

## Problem

An idle yserver desktop (no client repaint, no input, no cursor motion, HW cursor plane,
compositor OFF) composes + KMS-page-flips the **whole screen 60×/second forever**. GPU-independent
waste; ~free on AMD, a steady **7–10 % CPU** on discrete NVIDIA (per-atomic-commit kernel + driver
cost × 60/s — true self-time kernel ~2 % + `[nvidia]` ~1 %; the earlier "libc memcpy 4 %" reading
was a children%/self% mix-up and is NOT the cost).

Confirmed on:
- **Peter's GTX 1060**, single 3440×1440 — the reported 7–10 % idle CPU.
- **silence RX580**, dual 2560×1440 — reproduces (cheap, so never noticed).

### Evidence (fvwm3, `YSERVER_LOOP_TELEMETRY=1`, idle)
- `render_telemetry`: `frame_present_count/s=60`, `full_redraw_fallback/s=60`,
  `composite_submits/s=60`, `damage_fraction=1.000`; **28 s** with `paint_submits/s=0` yet
  `composite=60`. Loop: **24 s** of `req/s=0 page_flip/s=60`, `host_input/s=0`.
- Submit trace: `scene_compose` on output 0 every **16.7 ms** exactly.
- Diagnostic build (`diag/idle-present-compose-trigger`): `idle-compose-diag` shows
  `structure_dirty=true` every frame and culprit drawables with **constant** presentation-damage
  epochs (e.g. `(402, epoch 29, 29 rects)` unchanged for 14 s). `tick-clear-diag` shows
  `composed=1 clear_dirty=false n_outputs=2` steady on silence. `idle-dirty-bt` shows
  `wake_for_damage` fires ONLY at startup (run.rs:342/656) — it is a **never-clear**, not a re-arm.

## Root cause — two symptoms of one design flaw

The "needs compose" decision is a **global** signal that only clears when **all**
outputs/snapshots clear at once:

```
scene_wants_compose() = scene.scene_structure_dirty || store.has_pending_presentation_damage()
tick(): clear scene_structure_dirty only if EVERY output returned Composed | Skipped(EmptyDamage)
```

- **Cause 1 — multi-output never-clear (silence).** With ≥2 independently-phased outputs, at almost
  every tick one output is mid-flip and returns `Skipped(PendingAcks)` (a **non-clearing** skip, by
  design so its deferred damage survives). `clear_dirty &= false` → the global
  `scene_structure_dirty` **never clears** → every output free-runs.
- **Cause 2 — presentation-damage drain asymmetry (both boxes; sole cause on Peter's single
  output).** `has_pending_presentation_damage()` counts **all** scene-participating drawables with
  non-empty damage, but presentation damage is only ack'd/drained for drawables **actually drawn**
  in the scene walk (`peek_presentation_damage` is called only where a `CompositeDraw` is emitted —
  scene.rs ~2122 / ~2773, guarded by a live `image_view` / `emitted_any`). A scene-participating
  drawable that painted but isn't drawn this frame (off-screen, occluded, clipped out, or null GPU
  backing) holds damage that can **never** be ack'd → the predicate is perpetually true → free-run.
  Commit `01656908` (xfce-submenu fix) forces a full compose when *drawn-but-empty-projection*
  snapshots exist — a different set — and does not address the *never-drawn* case; where it does
  fire, its "self-limiting once acked" assumption fails whenever the same drawable re-qualifies.

Fixing Cause 2 likely subsumes Cause 1 at idle: with no spurious composes there are no flips, so no
`PendingAcks`, so the global-clear never gets blocked. Cause 1 is still fixed for correctness so a
*legitimate* single-output update can't spin the other output.

## Proposed fix

1. **Drain-consistency (Cause 2).** Make the compose-need predicate and the ack agree. Options
   (pick during impl, codex to weigh):
   - (a) `has_pending_presentation_damage()` counts a drawable only if it will actually be composed
     (drawn + live backing) — i.e. the same gate the peek uses; **or**
   - (b) when the scene walk determines a scene-participating drawable is not drawn this frame,
     drain (clear) its presentation damage (it cannot appear on screen, so retaining it is
     pointless) — bumping epoch so a later real paint re-arms.
   Must NOT strand a genuinely-visible late paint (the #01656908 submenu case): a drawable that
   *is* drawn but projects empty must still force one compose and then ack.
2. **Per-output compose-need (Cause 1).** Base "needs compose" on **per-output** state (each
   output's `scene_structure_damage` + presentation damage that projects onto it), and clear
   per-output, instead of one global bool that all outputs must agree to clear. An output that is
   mid-flip (`PendingAcks`) keeps only its **own** deferred damage; it must not force other outputs
   to recompose. `scene_wants_compose()` = "any output needs compose"; `next_wakeup` and
   `maybe_composite` must keep using the identical predicate (the busy-spin / stranded-paint
   constraint from the existing comment).

## Cut 2b — chosen implementation (option a, "not-projected" flag)

HW-confirmed culprits are all `Window:mapped` that clip to an empty visible box in
`build_scene` (off-screen / clipped-out helper windows — e.g. fvwm3 module or wezterm IME
windows): `emitted_any` stays false (scene.rs ~2735/2761), so they're never `peek`'d and their
presentation damage is never ack'd, yet `has_pending_presentation_damage()` counts them → idle
scheduler spins (~900 iter/s, no GPU work).

Design (preserves damage; does NOT clear it):
- Add `offscreen_no_draw: bool` to `Drawable` (default false).
- `tick_one_output` accumulates `built.sampled_ids` (drawables it actually drew) into a
  tick-level `drawn: HashSet<DrawableId>` and flags its output as *walked* (build_scene ran — i.e.
  not returned before the walk by the `PendingAcks`/`RetryDeadline` gate).
- After `tick()`'s per-output loop, **only if every output walked** (complete cross-output
  visibility), reconcile: for each scene-participating drawable, `offscreen_no_draw =
  has_damage && !drawn.contains(id)` (drawn → false; damaged-but-undrawn-everywhere → true).
  Gating on all-walked avoids mis-flagging a window that is visible only on an output that was
  mid-flip this tick (the multi-output transient edge).
- `has_pending_presentation_damage()` counts only `!offscreen_no_draw` drawables → off-screen
  windows no longer arm/​spin the compose scheduler.
- The flag is **not** touched by `damage()` (a client painting an off-screen window doesn't make
  it visible). A window becomes visible only via map/move/restack/configure — all of which already
  set `scene_structure_dirty`, forcing a full recompose → `tick` re-walks → draws it (Repaint::Full
  redraws from storage, so no lost paint) → `peek`+ack drains + clears the flag → its damage counts
  again. So no stranding.
- **Parity:** `next_wakeup` and `maybe_composite` both read `scene_wants_compose()` (flag-aware);
  the flag mutates only in `tick()`, after `next_wakeup` for the iteration was computed, so the two
  sites stay consistent within an iteration.
- **Required amendment (codex):** a mapped, on-screen window can also emit no draw when its X
  **bounding SHAPE** clips to empty (scene.rs ~2697). `set_shape_rectangles` (backend.rs ~17723)
  mutates `shape_bounding`/`shape_clip` WITHOUT waking the scene, so a flagged window whose shape
  later becomes non-empty would strand. Fix: `set_shape_rectangles` must `mark_scene_structure_dirty()`
  for `kind == 0` (bounding) and `kind == 1` (clip) — both affect what is drawn (`kind == 2`
  input-shape does not). This is also a latent correctness fix (a shape change wasn't repainting
  until an unrelated event).
- **Invariant (codex):** the `drawn` set holds the *sampled source* `DrawableId`s
  (`build_scene` pushes the resolved source, which for a redirected Automatic window is its backing,
  not the window's own id). Compatible with the current W+B participation pairing; documented so a
  future redirect change doesn't silently mis-flag.
- **Deferred (tracked, not in cut 2b):** the multi-output presentation-ownership case (a window
  with fresh damage visible only on an output that stays mid-flip across ticks) — bounded and
  self-correcting once that output retires and re-walks; the full per-output ack ownership is the
  separate follow-up codex flagged.

## Invariants to preserve (regression guards)
- **#30 cursor gating:** stationary same-mode HW/SW cursor contributes no damage; cursor
  motion/sprite/mode changes still repaint with no trail / no stale cursor. `pick_repaint_region`
  tripwire test stays green (Repaint::Full unconditional today).
- **#01656908 submenu:** a window painted whose damage projects empty must still show (force one
  compose, then ack) — no "painted but not shown until you move".
- **Deferred output:** an output that skipped (flip in flight / retry backoff / no BO / no pool)
  must still recompose its pending damage once ready — no stranded frame.
- **DPMS-off / VT-away:** loop still sleeps (no busy-wake), restores on return (#30 part C).

## Acceptance (HW-gated — the real test)
- `just yserver-fvwm3-hw-telemetry`, idle: `frame_present_count/s` → **0** (was 60) on both silence
  (dual) and Peter's 1060 (single). Move mouse → tracks motion → returns to 0 at rest.
- Peter's 1060: idle `top` CPU drops from 7–10 % toward ~0–1 %.
- xfce submenu (the `01656908` case) still appears without needing a nudge.
- Dual-monitor: content on both screens updates correctly; drag across the seam works.
- 900+ lib tests + v2 acceptance green; `cargo +nightly fmt`, `cargo clippy --all-targets`.

## Open item
Pin the exact culprit sub-case (never-drawn vs drawn-empty-projection vs null-backing) by extending
the diagnostic to map `DrawableId → host_xid` + drawn/peeked flag, if the fix option choice needs
it. Current evidence (constant epochs, never ack'd) is consistent with the never-drawn asymmetry.
