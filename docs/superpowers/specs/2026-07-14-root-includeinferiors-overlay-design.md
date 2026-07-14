# Root `IncludeInferiors` front-buffer overlay (legacy XOR rubber-band under a compositor)

Date: 2026-07-14
Status: design — codex-reviewed (round 1), revised

## Problem

ImageMagick `import` (region screenshot) draws its selection rubber-band as an
**XOR `PolyRectangle` on the ROOT window with `subwindow-mode=IncludeInferiors`**
(GC `function=GXinvert`). On real Xorg this rectangle is visible while dragging,
even with a compositor running. On yserver it is **invisible under any
compositor** (Cinnamon/muffin, XFCE/xfwm4) and only works with no compositor
(MATE default).

Confirmed with a minimal probe (`scratchpad/xor_probe.c`: XOR `PolyRectangle`
on root, then `XGetImage` readback of the composited scanout):
- yserver + Cinnamon: readback **unchanged** after the draw (XOR trapped in
  root's occluded backing).
- Xorg + Cinnamon: readback **changed** immediately, reverted ~150 ms later.

### Mechanism

yserver is a compositing X server with a **per-window-backing** model: every
window (including root) has its own GPU storage; a scene compose reads the
backings and composites them into a scanout BO that is page-flipped. Under a
client compositor, the desktop is painted into the Composite Overlay Window
(COW)/stage, drawn on top of everything, so root's own backing is fully
occluded.

`walk_stroke_inferiors` (crates/yserver/src/kms/render/backend.rs ~7401)
emulates `IncludeInferiors` by walking child windows, but **deliberately skips
compositor-redirected (non-`scene_participating`) windows**, and the paint lands
in per-drawable backings via `emit_stroke_output` (~7491) / `fill_solid_rects`
(~7540). So a root+`IncludeInferiors` draw touches only root's backing, which is
occluded → invisible. On Xorg there is one shared framebuffer, so the draw
scribbles over the composite transiently (the compositor repaints later; the
client redraws every pointer motion). yserver has no shared framebuffer.

Affects every user of the **legacy root-`IncludeInferiors` idiom**: `import`,
`xgrabsc`, classic `scrot` rubber-band, `xmag` region-select, lightweight-WM
move/resize wireframes (twm, fvwm non-opaque, mwm, olvwm). Modern tools
(`slop`/`maim`, `flameshot`, `spectacle`, `gnome-screenshot`) draw into their
own override-redirect window and already work — NOT the target.

## Goal / scope

Make **core drawing to the root window with `IncludeInferiors` reach the
composited front buffer**, so these overlays are visible under a compositor,
matching Xorg's observable behavior.

- **In scope (Phase 1):** reversible logic ops `GXinvert` and `GXxor`, via
  strokes (`PolyLine`/`PolySegment`/`PolyRectangle`/`PolyArc`) and **solid**
  fills (`PolyFillRectangle`/`FillPoly`/`PolyFillArc` with `FillState::Solid`).
- **Out of scope (documented follow-ups):** `GXcopy` (needs retained pixels, no
  known consumer); patterned/tiled/stippled fills; `PolyText`/`ImageText`/
  `PutImage`/`CopyArea` to root; non-reversible ops (`GXclear`/`GXset`/`GXand`/
  `GXor`/…).

## Approaches considered

**A — write the live OnScreen scanout BO (rejected).** One-shot GPU logic-op
into the currently-scanned-out BO, no compose. Codex-rejected: turns a KMS-owned
BO into a mutable front buffer with no display-side sync (tearing; "shows
without a flip" is driver behavior, not a contract), and because yserver
recomposes+flips on **every** damage (not on a compositor schedule like Xorg),
the scribble is wiped constantly and XOR desyncs far more often than Xorg →
*more* remnants. Only viable as a narrow experimental hack.

**B — retained overlay applied last at compose (chosen).** Retained server
state, re-applied as the final step of every compose into the fresh BO before
flip. Deterministic across drivers/composes, no live-front-buffer write, no
tearing, survives recompose. Design below.

## Design (Approach B)

### 1. State

Retained overlay on the render/scene state, root-absolute coords:

```
struct RootOverlay {
    // xor_value -> active rects toggled by that value. Almost always one entry.
    // GXinvert normalizes to value = plane_mask; GXxor to (foreground & plane_mask).
    // NOT a RegionSet: exact-match toggle over a rect list (see below).
    xor_ops: HashMap<u32, Vec<Rect2D>>,
    owner_clients: HashSet<ClientId>,
}
```

**Why a rect list with exact-match toggle, not region symmetric-difference:**
`RegionSet` (store.rs ~463) is a capped `Vec<Rect2D>` with `union_with` and an
exact-match-only `subtract`; it has no symmetric-difference. But the rubber-band
idiom always pairs erase/draw with **identical geometry** (the client XOR-erases
exactly the rects it drew), so membership toggle by exact rect match coalesces a
drag to just the current outline. **Pixel-level overlap correctness (thick-line
corners, overlapping edges) is handled by the apply pass replaying `dst ^= value`
per rect sequentially** — two overlapping rects double-XOR the shared pixels,
which is exactly XOR semantics. So no region algebra is needed. If a client's
erase does not exactly match its draw, a remnant results — the same failure Xorg
exhibits, so this is faithful.

**Cap (safe degradation, NOT bbox-collapse):** bound `xor_ops` total rects to
avoid a Window-Maker-style storm. Do **not** reuse `RegionSet`'s bbox-collapse
(store.rs ~481): collapsing an active XOR overlay to its bounding box would
change visible pixels and break later exact-match erase symmetry (the client's
erase rects would no longer match → corruption/remnant). Instead, if the retained
rect count exceeds a generous cap (well above any real outline, e.g. a few
thousand), **clear the whole overlay and log** — the rubber band vanishes
(degraded) rather than corrupts. Normal outlines are a handful of thin rects, so
the cap only trips on a pathological/misbehaving client.

### 2. Capture — reroute root+`IncludeInferiors` draws

Intercept when `host_xid == root && current_subwindow_mode == IncludeInferiors`
and `current_function ∈ {GXinvert, GXxor}`:

- **Strokes:** in `emit_stroke_output` (backend.rs ~7491) — all of
  `PolyLine`/`PolySegment`/`PolyRectangle`/`PolyArc` flow through it with GC
  state available; `line_width>1` is already rasterized to rects here.
- **Fills:** in `fill_rects_honoring_fill_state` (backend.rs ~7998), **only for
  `FillState::Solid`** (patterned fills bypass `fill_solid_rects` into the CPU
  pattern path — out of scope Phase 1). Do not intercept at `fill_solid_rects`
  (too low; misses the fill-style dispatch).

For an intercepted op:
- Value = `current_function`-normalized, depth-masked exactly like
  `fill_solid_rects`: `plane_mask = current_plane_mask & depth_plane_mask(depth)`;
  `GXinvert → value = plane_mask`; `GXxor → value = foreground & plane_mask`.
- **Toggle** each rasterized rect into `xor_ops[value]` by exact match (present →
  remove; absent → insert). Drop the entry when its list empties.
- Record the client (`owner_clients`); inject overlay damage (see §4).
- Do **not** paint root's backing (never displayed; root GetImage reads the
  composited scanout — confirmed by the existing test at backend.rs ~26297 — so
  a screenshot includes the overlay, matching Xorg).

**Prerequisite plumbing:** the draw entry points currently ignore `_origin`
(backend.rs ~14468/14685); thread `OriginContext`/`ClientId` into the capture so
`owner_clients` can be populated.

### 3. Apply — final step of compose

The scanout image path: `record_command_buffer` (scene.rs ~2797) records the
scene render pass into the BO, then transitions the BO to `GENERAL` for KMS
(~3072). Insert the overlay pass **after the scene pass, before that final
`GENERAL` barrier** (BO image/view available as `bo.vk_image` / `bo.vk_image_view`,
~2998), so it flips atomically in the same submit.

`record_logic_fill` (vk/ops/fill.rs ~171) is NOT drop-in: it wants a
`DrawableImage`, does its own layout transition, and ends in
`SHADER_READ_ONLY_OPTIMAL`. **Add a scanout-target logic-fill variant** that
targets the BO image in its compose-time layout and XORs `dst ^= value` over the
given rects, with **server-alpha semantics** (`opaque_alpha=true` per
engine.rs ~3779 — depth-24-in-32bpp must preserve the alpha byte, not XOR it).

Per output, per `(value, rects)` entry: intersect each rect with the output
layout and shift root-absolute → output-local (same math as
`split_root_scanout_reads`, backend.rs ~9241), then record the XOR pass. Applied
against the freshly-composited scene each frame → tracks the live desktop,
survives recompose.

### 4. Damage integration (correctness-critical)

`wake_for_damage` only flips `scene_structure_dirty` (scene.rs ~669); an
overlay-only change would leave `output_damage` empty → `EmptyDamage` skip
(~1502) clears the dirty bit and the overlay never appears. So on every overlay
mutation, **inject output damage for the affected rects** via
`mark_scene_structure_damage_rects` (or a dedicated per-output overlay-damage
path) so a real compose runs. With `Repaint::Full` (loadOp=CLEAR each frame) the
overlay re-applies cleanly and cannot double-apply.

### 5. Lifetime / clearing

- Entries empty naturally as the client XOR-erases at drag end.
- **Owner-client disconnect:** add a backend disconnect hook (new trait method;
  `process_disconnect` has `&mut dyn Backend` but no such callback today,
  process_disconnect.rs ~81 / trait_def.rs ~1989) that clears the disconnecting
  client's contribution. Phase-1 simplification: single global overlay, any
  owner disconnect clears all (documented limitation).
- **RandR / output topology:** clear the overlay on root screen-size change and
  output add/remove/reconfigure — the region is root-absolute and
  layout-dependent, and a retained overlay surviving a topology change is wrong
  vs Xorg's transient behavior.
- On transition to empty, inject damage so the final compose removes the mark.

### 6. Multi-output

Region root-absolute; per output, intersect + shift at apply time (§3).

## Testing

**Unit (pure):**
- Exact-match toggle: draw R1; draw R2; erase R1 (identical rects) → active == R2;
  toggle same rect twice → empty.
- Value normalization + depth mask: `GXinvert → plane_mask`;
  `GXxor(F) → F & depth_plane_mask(depth)`; alpha byte preserved.
- Per-output intersection/shift, including a region spanning an inter-output gap.
- Mixed active values (`GXinvert` + `GXxor(F)` simultaneously) apply as two passes.
- Cap/bbox fallback trips at the rect limit.
- **Overlay-only damage scheduling:** an overlay mutation with no other damage
  must schedule a compose (not `EmptyDamage`-skip).
- **Disconnect clear:** owner disconnect empties the overlay + schedules a clear
  compose.

**HW smoke on silence (dual 2560×1440, RX580):**
- `scratchpad/xor_probe.c`: `after XOR draw` readback must now CHANGE.
- Real `import` region select under Cinnamon AND XFCE: rubber-band visible while
  dragging; final capture correct. Also `xmag` if handy.
- Regressions: MATE (no compositor) still works; a normal app's own-window XOR
  unaffected (only root+IncludeInferiors is rerouted).

## Prerequisites (implementation order)

1. Thread `OriginContext`/`ClientId` into the stroke/fill capture entry points.
2. Backend "client disconnected" hook, called from `process_disconnect`.
3. Scanout-target logic-fill (XOR) recorder variant (server-alpha, BO layout).
4. Overlay-damage injection path (not `wake_for_damage` alone).

## Risks / open questions

- **Exact-match toggle assumption.** Relies on clients XOR-erasing identical
  geometry (true for the idiom). Non-matching erase → remnant = Xorg-faithful.
- **Cap policy.** Clear-and-log on overflow (never bbox-collapse — that would
  corrupt visible pixels and break erase symmetry). Real outlines are a handful
  of thin rects, far under any sane cap.
- **Cursor.** HW cursor plane stays above the primary scanout — correct; do not
  bake cursor into the overlay.
- **Follow-ups:** `GXcopy` / patterned fills / text deferred until a real
  consumer appears.
