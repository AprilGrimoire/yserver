# Root `IncludeInferiors` Front-Buffer Overlay Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make legacy root-window `IncludeInferiors` XOR/invert drawing (ImageMagick `import` rubber-band, WM move/resize wireframes, `xmag`) visible under a compositor by re-applying it as a retained overlay at the end of each scene compose.

**Architecture:** A retained per-XOR-value rect list on `SceneCompositor` is toggled by root+`IncludeInferiors` reversible-logic draws (exact-match erase/draw pairing). Inside each compose's dynamic-rendering instance (after the scene draws, before `cmd_end_rendering`), a scanout-target XOR logic-fill draws each `(value, rects)` entry into the freshly-composited scanout BO, per output, using a scene-owned XOR pipeline (RGB write-mask, server-alpha-safe). Overlay mutations inject output damage so a compose actually runs; the overlay is cleared on owner-client disconnect and on RandR/topology changes.

**Tech Stack:** Rust, Vulkan (ash), Linux KMS/DRM. Spec: `docs/superpowers/specs/2026-07-14-root-includeinferiors-overlay-design.md`.

**Toolchain (per AGENTS.md):** `cargo +nightly fmt`; `cargo clippy --all-targets -- -D warnings`; `cargo test`. Branch: `feat/root-includeinferiors-overlay`.

---

## File structure

- **New:** `crates/yserver/src/kms/render/root_overlay.rs` — `RootOverlay` state + pure toggle/normalize/cap/per-output-split logic (unit-tested, no GPU).
- **Modify:** `crates/yserver/src/kms/render/backend.rs` — thread `OriginContext` into `emit_stroke_output`; intercept root+IncludeInferiors reversible draws in `emit_stroke_output` and `fill_rects_honoring_fill_state`; route to the overlay instead of root backing.
- **Modify:** `crates/yserver/src/kms/render/scene.rs` — own the `RootOverlay` on `SceneCompositor`; damage injection; a scene-owned XOR pipeline on `SceneCompositorInner`; record the apply draws inside the compose render instance (before `cmd_end_rendering`); clear-on-RandR/topology.
- **New:** `crates/yserver/src/kms/vk/ops/scanout_logic_fill.rs` — records XOR solid-quad draws into the ACTIVE compose rendering instance (no begin/end-render, no barriers), server-alpha-safe (RGB write-mask).
- **Modify:** `crates/yserver-core/src/backend/trait_def.rs` + `crates/yserver-core/src/core_loop/process_disconnect.rs` — add a `client_disconnected` backend hook + call it.
- **Modify:** `crates/yserver/src/kms/render/mod.rs` — `mod root_overlay;`.

---

## Task 1: `RootOverlay` state + pure logic

**Files:**
- Create: `crates/yserver/src/kms/render/root_overlay.rs`
- Modify: `crates/yserver/src/kms/render/mod.rs`

- [ ] **Step 1: Add the module declaration**

In `crates/yserver/src/kms/render/mod.rs`, add alongside the other `mod` lines:

```rust
pub(crate) mod root_overlay;
```

- [ ] **Step 2: Write the failing tests**

Create `crates/yserver/src/kms/render/root_overlay.rs`:

```rust
//! Retained front-buffer overlay for legacy root-window `IncludeInferiors`
//! XOR/invert drawing (import rubber-band, WM wireframes). See
//! docs/superpowers/specs/2026-07-14-root-includeinferiors-overlay-design.md.

use ash::vk;
use std::collections::HashMap;
use yserver_protocol::x11::ClientId;

/// Max retained rects before we give up and clear (safe degradation — never
/// bbox-collapse an active XOR overlay: that changes visible pixels and breaks
/// exact-match erase symmetry). Real outlines are a handful of thin rects.
const MAX_OVERLAY_RECTS: usize = 4096;

#[derive(Default)]
pub(crate) struct RootOverlay {
    /// xor_value -> active rects toggled by that value (root-absolute coords).
    xor_ops: HashMap<u32, Vec<vk::Rect2D>>,
    owner_clients: std::collections::HashSet<ClientId>,
}

impl RootOverlay {
    pub(crate) fn is_empty(&self) -> bool {
        self.xor_ops.is_empty()
    }

    /// Toggle a batch of rects for one xor_value by EXACT match (present ->
    /// remove, absent -> insert). Records the owner. Returns true if state
    /// changed. On cap overflow, clears everything and returns true.
    pub(crate) fn toggle(
        &mut self,
        client: ClientId,
        value: u32,
        rects: &[vk::Rect2D],
    ) -> bool {
        if rects.is_empty() {
            return false;
        }
        self.owner_clients.insert(client);
        let list = self.xor_ops.entry(value).or_default();
        for r in rects {
            if let Some(pos) = list.iter().position(|e| rect_eq(e, r)) {
                list.swap_remove(pos);
            } else {
                list.push(*r);
            }
        }
        if list.is_empty() {
            self.xor_ops.remove(&value);
        }
        if self.total_rects() > MAX_OVERLAY_RECTS {
            log::warn!(
                "root overlay exceeded {MAX_OVERLAY_RECTS} rects; clearing (misbehaving client)"
            );
            self.clear();
        }
        true
    }

    fn total_rects(&self) -> usize {
        self.xor_ops.values().map(Vec::len).sum()
    }

    /// Clear the whole overlay (RandR/topology change, cap overflow).
    pub(crate) fn clear(&mut self) {
        self.xor_ops.clear();
        self.owner_clients.clear();
    }

    /// Drop one client's contribution on disconnect. Phase-1 simplification:
    /// if the disconnecting client was an owner, clear the whole overlay
    /// (single global overlay; multi-client root-XOR is rare).
    pub(crate) fn on_client_disconnect(&mut self, client: ClientId) -> bool {
        if self.owner_clients.contains(&client) {
            self.clear();
            true
        } else {
            false
        }
    }

    /// All root-absolute rects across every value (for damage injection).
    pub(crate) fn all_rects(&self) -> Vec<vk::Rect2D> {
        self.xor_ops.values().flatten().copied().collect()
    }

    /// Per-output apply list: for each (value, rect) intersecting `output`
    /// (root-absolute x,y,w,h), the output-LOCAL rect and its xor value.
    pub(crate) fn apply_list_for_output(
        &self,
        output: (i32, i32, u32, u32),
    ) -> Vec<(u32, vk::Rect2D)> {
        let (ox, oy, ow, oh) = output;
        let mut out = Vec::new();
        for (value, rects) in &self.xor_ops {
            for r in rects {
                if let Some(local) = intersect_to_local(*r, ox, oy, ow, oh) {
                    out.push((*value, local));
                }
            }
        }
        out
    }
}

fn rect_eq(a: &vk::Rect2D, b: &vk::Rect2D) -> bool {
    a.offset.x == b.offset.x
        && a.offset.y == b.offset.y
        && a.extent.width == b.extent.width
        && a.extent.height == b.extent.height
}

/// Intersect a root-absolute rect with an output rect; return the intersection
/// in output-LOCAL coords, or None if disjoint.
fn intersect_to_local(
    r: vk::Rect2D,
    ox: i32,
    oy: i32,
    ow: u32,
    oh: u32,
) -> Option<vk::Rect2D> {
    let rx0 = r.offset.x;
    let ry0 = r.offset.y;
    let rx1 = rx0 + r.extent.width as i32;
    let ry1 = ry0 + r.extent.height as i32;
    let ix0 = rx0.max(ox);
    let iy0 = ry0.max(oy);
    let ix1 = rx1.min(ox + ow as i32);
    let iy1 = ry1.min(oy + oh as i32);
    if ix0 >= ix1 || iy0 >= iy1 {
        return None;
    }
    Some(vk::Rect2D {
        offset: vk::Offset2D { x: ix0 - ox, y: iy0 - oy },
        extent: vk::Extent2D {
            width: (ix1 - ix0) as u32,
            height: (iy1 - iy0) as u32,
        },
    })
}

/// Normalize a reversible GC function + foreground + depth-plane-mask to the
/// per-pixel XOR value. GXinvert ignores fg (`dst = ~dst = dst ^ plane_mask`);
/// GXxor uses `dst ^= fg`. Returns None for non-reversible functions.
pub(crate) fn xor_value_for(
    function: yserver_core::backend::GcFunction,
    foreground: u32,
    plane_mask: u32,
) -> Option<u32> {
    use yserver_core::backend::GcFunction;
    match function {
        GcFunction::Invert => Some(plane_mask),
        GcFunction::Xor => Some(foreground & plane_mask),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(x: i32, y: i32, w: u32, h: u32) -> vk::Rect2D {
        vk::Rect2D {
            offset: vk::Offset2D { x, y },
            extent: vk::Extent2D { width: w, height: h },
        }
    }
    const C: ClientId = ClientId(7);

    #[test]
    fn toggle_draw_then_identical_erase_is_empty() {
        let mut o = RootOverlay::default();
        o.toggle(C, 0xffffff, &[r(10, 10, 100, 1), r(10, 10, 1, 80)]);
        assert!(!o.is_empty());
        // erase = same rects again
        o.toggle(C, 0xffffff, &[r(10, 10, 100, 1), r(10, 10, 1, 80)]);
        assert!(o.is_empty(), "identical erase cancels draw");
    }

    #[test]
    fn toggle_erase_old_draw_new_nets_new() {
        let mut o = RootOverlay::default();
        let old = r(10, 10, 100, 1);
        let new = r(10, 10, 120, 1);
        o.toggle(C, 0xffffff, &[old]);
        o.toggle(C, 0xffffff, &[old, new]); // erase old + draw new in one motion
        let rects = o.all_rects();
        assert_eq!(rects, vec![new], "net is the new rect only");
    }

    #[test]
    fn xor_value_normalization() {
        use yserver_core::backend::GcFunction;
        assert_eq!(xor_value_for(GcFunction::Invert, 0x123456, 0xffffff), Some(0xffffff));
        assert_eq!(xor_value_for(GcFunction::Xor, 0x12345678, 0xffffff), Some(0x345678));
        assert_eq!(xor_value_for(GcFunction::Copy, 0xffffff, 0xffffff), None);
    }

    #[test]
    fn apply_list_splits_per_output_to_local() {
        let mut o = RootOverlay::default();
        // a rect straddling the seam between DP-1 (0..2560) and HDMI-1 (2560..5120)
        o.toggle(C, 0xffffff, &[r(2500, 100, 200, 2)]);
        let left = o.apply_list_for_output((0, 0, 2560, 1440));
        let right = o.apply_list_for_output((2560, 0, 2560, 1440));
        assert_eq!(left, vec![(0xffffff, r(2500, 100, 60, 2))]);
        assert_eq!(right, vec![(0xffffff, r(0, 100, 140, 2))]);
    }

    #[test]
    fn disconnect_owner_clears() {
        let mut o = RootOverlay::default();
        o.toggle(C, 0xffffff, &[r(0, 0, 10, 10)]);
        assert!(o.on_client_disconnect(ClientId(99)) == false);
        assert!(!o.is_empty());
        assert!(o.on_client_disconnect(C));
        assert!(o.is_empty());
    }

    #[test]
    fn cap_overflow_clears() {
        let mut o = RootOverlay::default();
        let many: Vec<_> = (0..MAX_OVERLAY_RECTS as i32 + 10)
            .map(|i| r(i, 0, 1, 1))
            .collect();
        o.toggle(C, 0xffffff, &many);
        assert!(o.is_empty(), "overflow clears rather than bbox-collapsing");
    }
}
```

- [ ] **Step 3: Run tests, expect FAIL (module/type not wired)**

Run: `cargo test -p yserver --lib root_overlay 2>&1 | tail -20`
Expected: compile error or FAIL until the module compiles; iterate until the six tests pass.

- [ ] **Step 4: Run tests, expect PASS**

Run: `cargo test -p yserver --lib root_overlay 2>&1 | tail -20`
Expected: `test result: ok. 6 passed`

- [ ] **Step 5: fmt + clippy + commit**

```bash
cargo +nightly fmt
cargo clippy --all-targets -- -D warnings 2>&1 | tail -5
git add crates/yserver/src/kms/render/root_overlay.rs crates/yserver/src/kms/render/mod.rs
git commit -m "feat(overlay): RootOverlay state + toggle/normalize/split logic"
```

---

## Task 2: Own the overlay on `SceneCompositor` + damage injection

**Files:**
- Modify: `crates/yserver/src/kms/render/scene.rs`

- [ ] **Step 1: Add the field**

In the `SceneCompositor` struct definition in `scene.rs`, add:

```rust
    pub(crate) root_overlay: super::root_overlay::RootOverlay,
```

Initialize it in every `SceneCompositor` constructor with `super::root_overlay::RootOverlay::default()`. (Grep `SceneCompositor {` for constructors; there is the main `new` plus test fixtures.)

- [ ] **Step 2: Add a mutation entry point that also injects damage**

Add to `impl SceneCompositor`:

```rust
    /// Toggle an overlay XOR op (root-absolute rects) and inject output damage
    /// so a compose actually runs (wake_for_damage alone leaves output_damage
    /// empty and the frame is EmptyDamage-skipped).
    pub(crate) fn root_overlay_toggle(
        &mut self,
        client: yserver_protocol::x11::ClientId,
        value: u32,
        rects: &[ash::vk::Rect2D],
    ) {
        if self.root_overlay.toggle(client, value, rects) {
            // Damage BOTH the just-toggled rects and whatever remains, so the
            // frame that removes an erased mark still repaints it.
            let mut dmg = rects.to_vec();
            dmg.extend(self.root_overlay.all_rects());
            self.mark_scene_structure_damage_rects(&dmg);
            self.wake_for_damage();
        }
    }

    /// Clear the overlay (RandR/topology change) and damage the vacated rects.
    pub(crate) fn root_overlay_clear(&mut self) {
        if self.root_overlay.is_empty() {
            return;
        }
        let vacated = self.root_overlay.all_rects();
        self.root_overlay.clear();
        self.mark_scene_structure_damage_rects(&vacated);
        self.wake_for_damage();
    }

    /// Drop a disconnecting client's overlay contribution.
    pub(crate) fn root_overlay_on_disconnect(
        &mut self,
        client: yserver_protocol::x11::ClientId,
    ) {
        let vacated = self.root_overlay.all_rects();
        if self.root_overlay.on_client_disconnect(client) {
            self.mark_scene_structure_damage_rects(&vacated);
            self.wake_for_damage();
        }
    }
```

- [ ] **Step 3: Test — overlay mutation schedules a non-empty damage**

Add to `scene.rs` `#[cfg(test)] mod tests` (use the existing stub `SceneCompositor` fixture pattern near `mark_scene_structure_damage_rects_sets_dirty_on_stub`, scene.rs ~3382):

```rust
    #[test]
    fn root_overlay_toggle_marks_structure_damage() {
        let mut sc = /* existing stub SceneCompositor constructor used by
                        mark_scene_structure_damage_rects_sets_dirty_on_stub */;
        assert!(!sc.scene_structure_dirty);
        sc.root_overlay_toggle(
            yserver_protocol::x11::ClientId(1),
            0xffffff,
            &[ash::vk::Rect2D {
                offset: ash::vk::Offset2D { x: 5, y: 5 },
                extent: ash::vk::Extent2D { width: 20, height: 20 },
            }],
        );
        assert!(sc.scene_structure_dirty, "overlay mutation must mark structure damage");
        assert!(!sc.root_overlay.is_empty());
    }
```

Match the exact stub constructor used by the sibling test — read `mark_scene_structure_damage_rects_sets_dirty_on_stub` first and copy its setup verbatim.

- [ ] **Step 4: Run, expect PASS**

Run: `cargo test -p yserver --lib root_overlay_toggle_marks_structure_damage 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 5: fmt + clippy + commit**

```bash
cargo +nightly fmt && cargo clippy --all-targets -- -D warnings 2>&1 | tail -3
git add crates/yserver/src/kms/render/scene.rs
git commit -m "feat(overlay): own RootOverlay on SceneCompositor with damage injection"
```

---

## Task 3: Thread `OriginContext` into the stroke emit path

**Files:**
- Modify: `crates/yserver/src/kms/render/backend.rs`

Rationale: capture needs `client_id`; `poly_rectangle`/`poly_arc`/`poly_line`/`poly_segment` receive `_origin: Option<OriginContext>` but drop it before `emit_stroke_output`.

- [ ] **Step 1: Add an `origin` parameter to `emit_stroke_output`**

Change the signature (backend.rs ~7491) from:

```rust
    fn emit_stroke_output(
        &mut self,
        host_xid: u32,
        target: PaintTarget,
        foreground: u32,
        background: u32,
        out: crate::kms::render::stroke::StrokeOutput,
    ) {
```

to add `origin: Option<OriginContext>,` as the second parameter.

- [ ] **Step 2: Pass origin from every caller**

The callers are the stroke request handlers (`poly_rectangle` ~14546, `poly_arc` ~14595, `poly_line`, `poly_segment`, and the two `emit_stroke_output` calls at ~14505/14542). Each already has `_origin: Option<OriginContext>` in scope — rename `_origin` to `origin` and pass it through. Example for `poly_rectangle`:

```rust
        self.emit_stroke_output(
            host_xid,
            origin,
            target,
            foreground,
            stroke.background,
            crate::kms::render::stroke::StrokeOutput { fg_rects, bg_rects },
        );
```

- [ ] **Step 3: Build (no behavior change yet)**

Run: `cargo build -p yserver 2>&1 | tail -5`
Expected: compiles (origin currently unused in `emit_stroke_output` — add `let _ = origin;` temporarily to avoid the unused warning, removed in Task 4).

- [ ] **Step 4: Commit**

```bash
cargo +nightly fmt
git add crates/yserver/src/kms/render/backend.rs
git commit -m "refactor(overlay): thread OriginContext into emit_stroke_output"
```

---

## Task 4: Capture — reroute root+IncludeInferiors reversible draws into the overlay

**Files:**
- Modify: `crates/yserver/src/kms/render/backend.rs`

- [ ] **Step 1: Add a capture helper on the backend**

Add near `emit_stroke_output`:

```rust
    /// True iff this draw is a reversible-logic (`GXinvert`/`GXxor`) op to the
    /// ROOT window with `IncludeInferiors`. Gate BEFORE looking at rects, so a
    /// background-only run (e.g. `LineStyle::DoubleDash`) still skips the
    /// backing paint even when the fg run is empty.
    fn is_root_overlay_draw(&self, host_xid: u32) -> bool {
        use yserver_core::backend::{GcFunction, SubwindowMode};
        host_xid == self.core.window_id
            && matches!(self.core.current_subwindow_mode, SubwindowMode::IncludeInferiors)
            && matches!(self.core.current_function, GcFunction::Invert | GcFunction::Xor)
    }

    /// Fold one color-run's rects into the front-buffer overlay. Assumes
    /// `is_root_overlay_draw(host_xid)` already held. No-op if no client or no
    /// non-empty rects. `color` is the run's fill value (fg for solid/fg runs,
    /// GC background for DoubleDash off-runs).
    fn capture_root_overlay(
        &mut self,
        origin: Option<OriginContext>,
        color: u32,
        rects: &[Rectangle16],
    ) {
        if rects.is_empty() {
            return;
        }
        let depth = self
            .store
            .lookup(self.core.window_id)
            .and_then(|id| self.store.get(id))
            .map_or(24, |d| d.depth);
        let plane_mask = self.core.current_plane_mask & depth_plane_mask(depth);
        let Some(value) = crate::kms::render::root_overlay::xor_value_for(
            self.core.current_function,
            color,
            plane_mask,
        ) else {
            return;
        };
        let Some(client) = origin.map(|o| o.client_id) else {
            return;
        };
        let vk_rects: Vec<ash::vk::Rect2D> = rects
            .iter()
            .filter(|r| r.width != 0 && r.height != 0)
            .map(|r| ash::vk::Rect2D {
                offset: ash::vk::Offset2D { x: i32::from(r.x), y: i32::from(r.y) },
                extent: ash::vk::Extent2D {
                    width: u32::from(r.width),
                    height: u32::from(r.height),
                },
            })
            .collect();
        if vk_rects.is_empty() {
            return;
        }
        self.scene.root_overlay_toggle(client, value, &vk_rects);
    }
```

- [ ] **Step 2: Hook it in `emit_stroke_output`**

At the top of `emit_stroke_output`, after computing `fg_clipped`/`bg_clipped` (backend.rs ~7509), before painting host/inferiors, add:

```rust
        // Legacy root-overlay idiom: reroute reversible root+IncludeInferiors
        // strokes to the compose-time front-buffer overlay instead of the
        // (occluded) root backing. BOTH fg and bg runs toggle; gate on the
        // op-kind predicate (not rect emptiness) so a bg-only DoubleDash run
        // still skips the backing paint.
        if self.is_root_overlay_draw(host_xid) {
            self.capture_root_overlay(origin, foreground, &fg_clipped);
            self.capture_root_overlay(origin, background, &bg_clipped);
            return;
        }
```

(`return` early — skip the backing paint entirely for captured ops.)

- [ ] **Step 2b: Hook it in `fill_rects_honoring_fill_state`**

In `fill_rects_honoring_fill_state` (backend.rs ~7998), after `include_inferiors`/`function` are known and ONLY for solid fills, add before the normal fill work. Solid-fill gate: check the current fill style is solid (read how `fill_rects_honoring_fill_state` currently distinguishes solid vs pattern — it dispatches patterned fills to `fill_pattern_rects_cpu_fallback`; capture only when it would take the SOLID path). The capture call itself is the same:

```rust
        if self.is_root_overlay_draw(host_xid) {
            self.capture_root_overlay(origin, fg, rects);
            return;
        }
```

Fills have no bg run, so a single `capture_root_overlay` call suffices. Place this ONLY on the `FillState::Solid` branch (patterned fills route to `fill_pattern_rects_cpu_fallback` and are out of scope). `fill_rects_honoring_fill_state` must also receive `origin` (thread it from its callers `poly_fill_rectangle`/`fill_poly`/`poly_fill_arc`/`fill_rectangle`, same as Task 3). Add the `origin` param and pass through.

- [ ] **Step 3: Remove the temporary `let _ = origin;` from Task 3.**

- [ ] **Step 4: Unit test — capture routes to overlay, not backing**

Add to backend.rs tests (use the existing non-Vk `KmsBackend::for_tests` fixture and the `dispatch_poly_fill_rectangle` helper used by `root_get_image_reads_scanout_pixels_not_root_storage`'s neighbors):

```rust
    #[test]
    fn root_include_inferiors_xor_routes_to_overlay() {
        let mut b = KmsBackend::for_tests();
        // set GC: function=Invert, subwindow_mode=IncludeInferiors via the
        // same apply_draw_state path the request handlers use (mirror an
        // existing test that sets current_function/current_subwindow_mode).
        b.core.current_function = yserver_core::backend::GcFunction::Invert;
        b.core.current_subwindow_mode = yserver_core::backend::SubwindowMode::IncludeInferiors;
        let origin = Some(yserver_core::backend::OriginContext {
            client_id: yserver_protocol::x11::ClientId(3),
            nested_seq: 0,
            opcode: 70,
        });
        let rects = [Rectangle16 { x: 10, y: 10, width: 50, height: 1 }];
        let root = b.core.window_id;
        assert!(b.is_root_overlay_draw(root));
        b.capture_root_overlay(origin, !0, &rects);
        assert!(!b.scene.root_overlay.is_empty(), "op landed in overlay");
    }
```

- [ ] **Step 5: Run + fmt + clippy + commit**

```bash
cargo test -p yserver --lib root_include_inferiors_xor_routes_to_overlay 2>&1 | tail -20
cargo +nightly fmt && cargo clippy --all-targets -- -D warnings 2>&1 | tail -3
git add crates/yserver/src/kms/render/backend.rs
git commit -m "feat(overlay): capture root+IncludeInferiors reversible draws into overlay"
```

---

## Task 5: Backend `client_disconnected` hook + overlay clear on disconnect / RandR

**Files:**
- Modify: `crates/yserver-core/src/backend/trait_def.rs`
- Modify: `crates/yserver-core/src/core_loop/process_disconnect.rs`
- Modify: `crates/yserver/src/kms/render/backend.rs` (impl the hook)
- Modify: `crates/yserver/src/kms/render/scene.rs` (call `root_overlay_clear` from RandR/topology paths)

- [ ] **Step 1: Add the trait method (default no-op)**

In `trait_def.rs`, near the other lifecycle hooks (e.g. `release_glx_pixmap_export`, ~1989):

```rust
    /// A client fully disconnected. Backends that hold per-client transient
    /// state (e.g. the root front-buffer overlay) drop that client's
    /// contribution here. Default no-op.
    fn client_disconnected(&mut self, _client_id: ClientId) {}
```

- [ ] **Step 2: Call it from `process_disconnect`**

In `process_disconnect.rs`, in the connection-tied teardown block (near the other `backend.*` cleanup calls, ~254), add:

```rust
    backend.client_disconnected(client_id);
```

- [ ] **Step 3: Implement the hook on `KmsBackend`**

In backend.rs, in the `impl Backend for KmsBackend` block:

```rust
    fn client_disconnected(&mut self, client_id: ClientId) {
        self.scene.root_overlay_on_disconnect(client_id);
    }
```

- [ ] **Step 4: Clear on RandR screen-size / output-topology change**

Find where the scene reacts to output-layout changes (grep `fire_randr_changes` / output rebuild / `outputs =` in scene.rs and backend.rs — the same code path that rebuilds `platform.outputs`). At each such reconfiguration, call `self.scene.root_overlay_clear();` (or `self.root_overlay_clear()` if inside SceneCompositor). Add a one-line comment referencing this plan.

- [ ] **Step 5: Test — disconnect hook clears overlay via backend**

```rust
    #[test]
    fn client_disconnected_clears_overlay() {
        let mut b = KmsBackend::for_tests();
        b.scene.root_overlay_toggle(
            yserver_protocol::x11::ClientId(5),
            0xffffff,
            &[ash::vk::Rect2D {
                offset: ash::vk::Offset2D { x: 0, y: 0 },
                extent: ash::vk::Extent2D { width: 4, height: 4 },
            }],
        );
        assert!(!b.scene.root_overlay.is_empty());
        yserver_core::backend::Backend::client_disconnected(
            &mut b,
            yserver_protocol::x11::ClientId(5),
        );
        assert!(b.scene.root_overlay.is_empty());
    }
```

- [ ] **Step 6: Run + fmt + clippy + full lib test + commit**

```bash
cargo test -p yserver --lib client_disconnected_clears_overlay 2>&1 | tail -20
cargo +nightly fmt && cargo clippy --all-targets -- -D warnings 2>&1 | tail -3
cargo test -p yserver --lib 2>&1 | tail -5   # nothing else regressed
git add -A
git commit -m "feat(overlay): clear overlay on client disconnect and RandR topology change"
```

---

## Task 6: Apply pass — scanout-target XOR logic-fill at end of compose

**Files:**
- Create: `crates/yserver/src/kms/vk/ops/scanout_logic_fill.rs`
- Modify: `crates/yserver/src/kms/vk/ops/mod.rs` (add `pub mod scanout_logic_fill;`)
- Modify: `crates/yserver/src/kms/render/scene.rs` (`record_compose` / `record_command_buffer`)

This is the one GPU task. It gets an `#[ignore]`d live-Vk test (Step 5, using `for_tests_with_vk_live_scene()`) as the pre-hardware gate, plus HW smoke (Task 7). It cannot be a plain unit test — there is no scanout BO in the non-Vk fixture.

- [ ] **Step 1: Read the reference implementation COMPLETELY before writing.**

Read, in full:
- `crates/yserver/src/kms/vk/ops/fill.rs::record_logic_fill` (~171) — the pattern to mirror (barriers, dynamic rendering / render pass, INVERT/XOR logic-op pipeline, per-rect scissored draw).
- The `LogicFillPipelineCache` it uses (grep `LogicFillPipelineCache`) — how an XOR/INVERT pipeline is obtained for a given format + logic op.
- `crates/yserver/src/kms/render/scene.rs` `record_compose` (~2797) and `record_command_buffer` (~2907) — how the scene pass records into `bo` and the final `COLOR_ATTACHMENT_OPTIMAL -> GENERAL` barrier (~3069-3079); note `bo.vk_image` / `bo.vk_image_view` (~2998) and the BO format.

- [ ] **Step 2: Add a scene-owned XOR logic-fill pipeline cache**

`RenderEngine`'s `LogicFillPipelineCache` is private and unreachable from `SceneCompositor`, so give the compositor its own. The real type (vk/logic_fill_pipeline.rs ~70) is:

```rust
LogicFillPipelineCache::new(vk: Arc<VkContext>, color_format: vk::Format) -> Result<Self, LogicFillError>
LogicFillPipelineCache::get(&mut self, function: GcFunction, opaque_alpha: bool) -> Result<vk::Pipeline, LogicFillError>
LogicFillPipelineCache::pipeline_layout(&self) -> vk::PipelineLayout
```

In `SceneCompositorInner` (scene.rs ~469, which already holds `vk: Arc<VkContext>` and `pipeline: CompositorPipeline`), add:

```rust
    overlay_xor_cache: crate::kms::vk::logic_fill_pipeline::LogicFillPipelineCache,
```

Construct it in the `SceneCompositorInner` constructor (scene.rs ~570):

```rust
    overlay_xor_cache: crate::kms::vk::logic_fill_pipeline::LogicFillPipelineCache::new(
        std::sync::Arc::clone(&vk),
        scanout_color_format, // the same vk::Format the compose color attachment uses
    )
    .map_err(/* map into the constructor's error type */)?,
```

Read the constructor to find the exact `vk::Format` the compose attachment/BO uses and reuse it. **We do NOT build a custom write-mask:** `get(GcFunction::Xor, /*opaque_alpha=*/ true)` already masks alpha out so the XOR only touches RGB (the server-α invariant on depth-24 — see the `opaque_alpha` doc on the cache). A single `Xor` pipeline serves both invert (value = plane_mask) and xor (value = fg).

- [ ] **Step 3: Write `record_scanout_logic_fill` (records INTO the active rendering)**

Create `crates/yserver/src/kms/vk/ops/scanout_logic_fill.rs` and register it in `crates/yserver/src/kms/vk/ops/mod.rs` (`pub mod scanout_logic_fill;`).

Critical: the overlay is recorded **inside the compose's existing dynamic-rendering instance** — between `cmd_begin_rendering` (scene.rs ~3013) and `cmd_end_rendering` (~3070) — NOT as a separate pass. So this function does **no** `cmd_begin_rendering`/`cmd_end_rendering` and **no** layout barriers (the BO is already `COLOR_ATTACHMENT_OPTIMAL` and rendering is active). It only: `cmd_bind_pipeline(pipeline)`, then per op set the scissor to the output-local rect, push the `LogicFillPushConsts` (the geometry + color; color = `xor_value` decoded to `[f32;4]` exactly as `fill_solid_rects` does via `decode_x11_pixel_for_storage`), and draw. Mirror the draw-recording half of `record_logic_fill` (fill.rs ~171) *after* its barrier/begin-render setup — reuse the same `LogicFillPushConsts` layout and vertex/draw call.

```rust
pub fn record_scanout_logic_fill(
    vk: &VkContext,
    cb: vk::CommandBuffer,
    pipeline: vk::Pipeline,
    pipeline_layout: vk::PipelineLayout,
    ops: &[(u32 /*xor_value*/, vk::Rect2D /*output-local scissor*/)],
);
```

(No `bo_extent` param — the compose viewport is already set for the active instance; each op only sets its own scissor.)

- [ ] **Step 4: Invoke it inside `record_command_buffer`, precomputing ops in the outer tick**

Ownership + borrow: `root_overlay` is on the OUTER `SceneCompositor`; the outer `tick()` (scene.rs ~620) currently holds `self.inner.as_mut()` across the per-output loop, so **restructure that borrow first** (shorten it, or destructure `let SceneCompositor { inner, root_overlay, .. } = self;` so the two fields are borrowed disjointly). The per-output layout comes from `platform.outputs[output_idx]` (scene.rs ~1622), reachable through `inner` immutably. Compute the apply list (owned `Vec`) and the pipeline/layout BEFORE the `&mut inner` call into `tick_one_output`:

```rust
    let overlay_ops = root_overlay.apply_list_for_output(
        (layout.x, layout.y, u32::from(layout.width), u32::from(layout.height)),
    );
    let xor_pipeline = inner.overlay_xor_cache
        .get(yserver_core::backend::GcFunction::Xor, true)?; // opaque_alpha=true → RGB-only
    let xor_layout = inner.overlay_xor_cache.pipeline_layout();
```

Thread `overlay_ops: &[(u32, vk::Rect2D)]`, `xor_pipeline: vk::Pipeline`, `xor_layout: vk::PipelineLayout` through `tick_one_output` (~1380) → `record_compose` (~2797) → `record_command_buffer` (~2907). Do NOT reference `inner` inside `record_command_buffer` (it isn't in scope there — it only gets `vk`, `bo`, `pipeline`, `scene`, `descriptors`, `repaint`). After the scene draws but BEFORE `cmd_end_rendering` (~3070):

```rust
    if !overlay_ops.is_empty() {
        crate::kms::vk::ops::scanout_logic_fill::record_scanout_logic_fill(
            vk, cb, xor_pipeline, xor_layout, overlay_ops,
        );
    }
```

- [ ] **Step 5: Ignored live-Vk test (pre-HW gate)**

A live scanout fixture exists — `KmsBackend::for_tests_with_vk_live_scene()` (backend.rs ~1782). Add an `#[ignore]`d test (runs only with a real ICD, like the neighbors at backend.rs ~26297 / render_acceptance.rs ~1658): set GC `function=Invert`, `subwindow_mode=IncludeInferiors`; dispatch a root `PolyRectangle`; drive one compose tick; `read_scanout_region`/root `GetImage` the drawn region and assert pixels differ from a pre-draw baseline (the XOR reached the composited scanout). Model setup on `root_get_image_reads_scanout_pixels_not_root_storage`.

Run: `cargo test -p yserver root_overlay -- --ignored 2>&1 | tail -20` on silence (real RADV). Expected: PASS.

- [ ] **Step 6: Build + fmt + clippy**

Run: `cargo build -p yserver 2>&1 | tail -5` then `cargo clippy --all-targets -- -D warnings 2>&1 | tail -5`
Expected: clean.

- [ ] **Step 7: Commit**

```bash
cargo +nightly fmt
git add -A
git commit -m "feat(overlay): apply retained root overlay as XOR pass at end of compose"
```

---

## Task 7: HW smoke on silence (the real gate)

**Files:** none (verification).

- [ ] **Step 1: Install the build**

Run: `just install` (release build + `sudo install` to `/usr/local/bin/yserver`), then restart the session so the new server runs. (This kills the Claude session; resume with `claude --resume yserver-silence`.)

- [ ] **Step 2: Probe — the minimal case must now change the composite**

Run under Cinnamon (or any compositor):
```fish
cd /home/jos/Projects/yserver/scratchpad
cc xor_probe.c -o xor_probe (pkg-config --libs x11); ./xor_probe
```
Expected: `after XOR draw: *** CHANGED (reached composite) ***` (was `unchanged` before the fix).

- [ ] **Step 3: Real import under a compositor**

Cinnamon AND XFCE: run `import /tmp/shot.png`, drag a region — the rubber-band rectangle must be visible while dragging; release; `/tmp/shot.png` is the correct region.

- [ ] **Step 4: Regressions**

- MATE (no compositor): import still works (unchanged path).
- A normal app drawing XOR into its OWN window still works (only root+IncludeInferiors is rerouted).
- No visible corruption of the desktop after a drag (overlay cleared; no stranded invert region).

- [ ] **Step 5: Squash-merge (with confirmation)**

Once smoke passes, ask the user before squash-merging `feat/root-includeinferiors-overlay` to master.

---

## Notes / deferred (from spec)

- `GXcopy` to root+IncludeInferiors (retained pixels, not toggle), patterned fills, `PolyText`/`ImageText`/`PutImage`/`CopyArea` to root — deferred, no known consumer.
- Phase-1 single global overlay: any owner disconnect clears all. Revisit if a real multi-client root-XOR case appears.
