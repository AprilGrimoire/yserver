# ImageMagick `import` region-select: honor IncludeInferiors for stroke ops (issue #90)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `import`'s XOR selection rectangle visible under yserver by extending the server's existing `IncludeInferiors` handling — currently implemented only for fill-family ops — to the stroke-family ops (`PolyRectangle`/`PolySegment`/`PolyLine`/`PolyArc`), in a way that is **XOR-safe** (paints each visible backing region exactly once).

**Architecture:** yserver composites each window from its own backing buffer bottom-to-top; a draw to root's own backing is occluded by every window over it. Xorg makes root `IncludeInferiors` draws visible via a single shared front buffer. yserver emulates this for **fill** ops by also painting into inferiors' backings (`fill_rects_honoring_fill_state` → `collect_fill_rects_for_inferiors`, backend.rs:7826/7265). Stroke ops (`import`'s `PolyRectangle`) never consult `subwindow_mode`, so they only hit root's occluded backing.

**Why not reuse the fill path verbatim (codex review, round 1):** `collect_fill_rects_for_inferiors` recurses into *every* mapped descendant, and `resolve_paint_target` (backend.rs:2088) routes a non-redirected descendant to its nearest redirected ancestor's backing. Under `GXcopy` (the only function fills use in practice) double-covering a backing is idempotent, so the aliasing is a *latent* bug there. Under `GXinvert` (exactly what `import` uses) painting the same backing pixels twice **cancels** — the rubber-band would have gaps where its outline crosses a sub-window boundary. So the stroke path needs a **redirect-boundary-aware, target-deduplicated** collection: paint each distinct resolved backing at most once (an ancestor's entry already covers a descendant that routes into the same backing), and skip non-`scene_participating` (manually-redirected) windows — which `collect_fill_rects_for_inferiors` does *not* do but `clip_fill_rects_by_subwindow_mode` (backend.rs:7060) does.

XOR itself already works for strokes: `fill_solid_rects` (backend.rs:7368) routes non-`Copy` functions through `engine.logic_fill` → `vk::LogicOp::INVERT`. No rop work needed.

**Rejected alternative:** a new topmost overlay surface (mirroring the COW). Over-engineered and inconsistent with the codebase's established "paint into inferiors" model. Not pursued.

**Tech Stack:** Rust, KMS/Vulkan renderer (`crates/yserver`), X11 core (`crates/yserver-core`). HW test box: **silence** (i9-13900K / RX 580, amdgpu+RADV). Software-Vulkan (lavapipe) for acceptance tests.

**Out of scope:** `poly_point` (backend.rs:14459) does not route through `emit_stroke_output` and is left unchanged — `import` does not use it, and single-pixel points under `IncludeInferiors` are a separate, lower-value gap. The grab-cursor crosshair (#90 symptom 2) is the OPTIONAL Task 5; it does not make `import` usable on its own.

---

## File Structure

- `crates/yserver/src/kms/render/backend.rs` — MODIFY. New method `collect_stroke_inferior_targets` (redirect-boundary-aware, target-deduped). `emit_stroke_output` (7351) gains a `host_xid` param + inferior-painting via the new method. Its four callers (`poly_line` ~14281, `poly_segment` ~14322, `poly_rectangle` 14359, `poly_arc` ~14407) pass `host_xid`. Reuses `resolve_paint_target` (2088), `fill_solid_rects` (7368), `intersect_with_current_clip_live` unchanged.
- `crates/yserver/tests/render_acceptance.rs` — MODIFY. Two `#[ignore = "needs live Vulkan ICD"]` pixel tests: (a) stroke on root with `IncludeInferiors` reaches a **redirected** top-level's backing; (b) XOR single-pass has no gap where the outline crosses a non-redirected child of a redirected window (the double-invert regression guard).
- `crates/yserver-core/src/core_loop/process_request.rs` — MODIFY (Task 5 only, optional).

---

### Task 1: Failing CI unit test for the dedup contract (no Vulkan)

**Files:**
- Test: `crates/yserver/src/kms/render/backend.rs` — add to the existing `#[cfg(test)] mod` alongside the `resolve_paint_target_*` tests (~23235). Reuse the same fixture builders those tests use to construct a `KmsBackend`, insert `windows` entries, and set redirected targets via `store.set_redirected_target` / `test_set_redirected_target`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn stroke_inferior_targets_dedup_redirected_ancestor() {
    // Tree: root -> W (redirected to backing B) -> C (NOT redirected).
    // Both W and C resolve to B. A root stroke crossing both must yield
    // B exactly ONCE (C is covered by W's entry) so XOR paints once.
    let mut be = /* same KmsBackend fixture as resolve_paint_target_* tests */;
    let root = be.core.window_id;
    let w = 0x0060_0001u32;
    let c = 0x0060_0002u32;
    let backing = 0x0060_00ffu32;
    insert_mapped_window(&mut be, w, /*parent*/ None,       0,   0, 400, 400); // top-level
    insert_mapped_window(&mut be, c, /*parent*/ Some(w),   50,  50, 100, 100);
    alloc_backing_pixmap(&mut be, backing, 400, 400);
    assert!(be.test_set_redirected_target(w, backing));

    // Stroke rects in root-local coords, crossing both W and C.
    let rects = vec![Rectangle16 { x: 0, y: 0, width: 400, height: 2 }];
    let targets = be.collect_stroke_inferior_targets(root, &rects);

    let backing_id = be.store.lookup(backing).unwrap();
    let hits: Vec<_> = targets.iter().filter(|(t, _)| t.id == backing_id).collect();
    assert_eq!(hits.len(), 1, "redirected backing B must appear exactly once (XOR-safe), got {}", hits.len());
}
```

> Replace the `/* fixture */` and `insert_mapped_window` / `alloc_backing_pixmap` lines with the exact helpers used by the neighbouring `resolve_paint_target_*` tests (confirm their names by reading that test module before writing). Do not invent helpers — reuse the existing ones.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p yserver --lib stroke_inferior_targets_dedup`
Expected: FAIL to compile — `collect_stroke_inferior_targets` does not exist yet.

---

### Task 2: Implement `collect_stroke_inferior_targets` + wire into `emit_stroke_output`

**Files:**
- Modify: `crates/yserver/src/kms/render/backend.rs`

- [ ] **Step 1: Add the redirect-boundary-aware collection method**

Place near `collect_fill_rects_for_inferiors` (backend.rs:7265):

```rust
/// XOR-safe inferior collection for `IncludeInferiors` stroke painting.
///
/// Unlike `collect_fill_rects_for_inferiors` (which recurses into every
/// mapped descendant and can double-cover a backing — harmless under
/// idempotent `GXcopy` fills, but under `GXinvert` a second pass over the
/// same backing pixels CANCELS the first), this resolves each contributing
/// window to its `PaintTarget` and emits each distinct backing at most
/// once. A descendant that routes into a backing already covered by an
/// ancestor is skipped (the ancestor's entry covers those pixels); the walk
/// still recurses to find independently-redirected descendants (distinct
/// backings). Non-`scene_participating` (manually-redirected) windows are
/// skipped entirely, matching `clip_fill_rects_by_subwindow_mode`.
///
/// `rects` and the returned rects are in each window's LOCAL coordinates;
/// `fill_solid_rects` applies the `PaintTarget.offset` into the backing.
fn collect_stroke_inferior_targets(
    &self,
    host_xid: u32,
    rects: &[Rectangle16],
) -> Vec<(PaintTarget, Vec<Rectangle16>)> {
    use std::collections::HashSet;
    let mut seen: HashSet<super::store::DrawableId> = HashSet::new();
    // Seed with the host's own target so an inferior routing into the host
    // backing (e.g. a top-level when root itself is redirected) is skipped.
    if let Some(t) = self.resolve_paint_target(host_xid) {
        seen.insert(t.id);
    }
    let mut out = Vec::new();
    self.walk_stroke_inferiors(host_xid, rects, &mut seen, &mut out);
    out
}

fn walk_stroke_inferiors(
    &self,
    parent_xid: u32,
    rects: &[Rectangle16],
    seen: &mut std::collections::HashSet<super::store::DrawableId>,
    out: &mut Vec<(PaintTarget, Vec<Rectangle16>)>,
) {
    for (child_xid, geom) in &self.windows {
        let is_child = if parent_xid == self.core.window_id {
            geom.parent == Some(self.core.window_id) || geom.parent.is_none()
        } else {
            geom.parent == Some(parent_xid)
        };
        if !is_child || !geom.mapped {
            continue;
        }
        // Skip manually-redirected (non-participating) windows; their
        // backing is composited separately and not part of this draw.
        let participating = self
            .store
            .lookup(*child_xid)
            .and_then(|id| self.store.get(id))
            .is_some_and(|d| d.scene_participating);
        if !participating {
            continue;
        }
        // Intersect the parent-local rects with this child's geometry and
        // translate into child-local coords (same math as
        // `collect_fill_rects_for_inferiors`).
        let cx = i32::from(geom.x);
        let cy = i32::from(geom.y);
        let cw = i32::from(geom.width);
        let ch = i32::from(geom.height);
        let mut child_rects = Vec::new();
        for r in rects {
            let rx0 = i32::from(r.x);
            let ry0 = i32::from(r.y);
            let rx1 = rx0 + i32::from(r.width);
            let ry1 = ry0 + i32::from(r.height);
            let ix0 = rx0.max(cx);
            let iy0 = ry0.max(cy);
            let ix1 = rx1.min(cx + cw);
            let iy1 = ry1.min(cy + ch);
            if ix0 < ix1 && iy0 < iy1 {
                child_rects.push(Rectangle16 {
                    x: (ix0 - cx) as i16,
                    y: (iy0 - cy) as i16,
                    width: (ix1 - ix0) as u16,
                    height: (iy1 - iy0) as u16,
                });
            }
        }
        if child_rects.is_empty() {
            continue;
        }
        let Some(target) = self.resolve_paint_target(*child_xid) else {
            continue;
        };
        // Emit only when this child introduces a NEW backing; either way,
        // recurse to discover independently-redirected descendants.
        if seen.insert(target.id) {
            out.push((target, child_rects.clone()));
        }
        self.walk_stroke_inferiors(*child_xid, &child_rects, seen, out);
    }
}
```

- [ ] **Step 2: Route `emit_stroke_output` through it**

Replace `emit_stroke_output` (backend.rs:7351):

```rust
fn emit_stroke_output(
    &mut self,
    host_xid: u32,
    target: PaintTarget,
    foreground: u32,
    background: u32,
    out: crate::kms::render::stroke::StrokeOutput,
) {
    let include_inferiors = matches!(
        self.core.current_subwindow_mode,
        yserver_core::backend::SubwindowMode::IncludeInferiors,
    ) && (self.windows.contains_key(&host_xid) || host_xid == self.core.window_id);

    // Apply the GC clip FIRST, then collect inferiors from the clipped rects.
    // The clip-mask applies to drawing into inferiors too (X11 semantics), and
    // this matches the fill path where callers pre-clip before
    // `fill_rects_honoring_fill_state` collects inferiors (codex round-2).
    let fg_clipped = self.intersect_with_current_clip_live(&out.fg_rects);
    let bg_clipped = self.intersect_with_current_clip_live(&out.bg_rects);

    let fg_inferiors = if include_inferiors {
        self.collect_stroke_inferior_targets(host_xid, &fg_clipped)
    } else {
        Vec::new()
    };
    let bg_inferiors = if include_inferiors {
        self.collect_stroke_inferior_targets(host_xid, &bg_clipped)
    } else {
        Vec::new()
    };

    // Host's own backing.
    if !fg_clipped.is_empty() {
        self.fill_solid_rects(target, foreground, &fg_clipped);
    }
    if !bg_clipped.is_empty() {
        self.fill_solid_rects(target, background, &bg_clipped);
    }

    // Each distinct inferior backing, exactly once (XOR-safe).
    for (child_target, child_rects) in fg_inferiors {
        self.fill_solid_rects(child_target, foreground, &child_rects);
    }
    for (child_target, child_rects) in bg_inferiors {
        self.fill_solid_rects(child_target, background, &child_rects);
    }
}
```

- [ ] **Step 3: Update the four call sites** to pass `host_xid` first. Each caller already has `host_xid`:

```rust
self.emit_stroke_output(
    host_xid,
    target,
    foreground,
    stroke.background,
    crate::kms::render::stroke::StrokeOutput { fg_rects, bg_rects },
);
```

Verify: `rg -n 'emit_stroke_output\(' crates/yserver/src/kms/render/backend.rs` — every call passes `host_xid` first, and no other callers exist.

- [ ] **Step 4: Run the Task 1 unit test → PASS**

Run: `cargo test -p yserver --lib stroke_inferior_targets_dedup`
Expected: PASS.

- [ ] **Step 5: Toolchain gates (exact CI commands, per AGENTS.md)**

Run: `cargo +nightly fmt && cargo clippy --all-targets -- -D warnings && cargo test -p yserver --lib`
Expected: no warnings; tests green.

- [ ] **Step 6: Commit** (do NOT push — Task 6 HW smoke gates the push)

```bash
git add crates/yserver/src/kms/render/backend.rs
git commit -m "fix(render): XOR-safe IncludeInferiors for stroke ops (import selection rect) (#90)"
```

---

### Task 3: Acceptance test — stroke on root reaches a redirected top-level's backing

**Files:**
- Test: `crates/yserver/tests/render_acceptance.rs` (append). Model on `root_fill_with_include_inferiors_matches_top_level_result` (line 616) for the window/child construction and on `fill_tiled_xor_with_reversed_tile_matches_solid_xor` (line 520) for the `apply_draw_state` shape. Use `GXcopy` here (visibility check); XOR parity is Task 4.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
#[ignore = "needs live Vulkan ICD"]
fn stroke_on_root_include_inferiors_reaches_redirected_topluvel_backing() {
    use yserver_core::backend::WindowHandle;
    use yserver_core::host_x11::HostSubwindowVisual;
    let mut b = match KmsBackend::for_tests_with_vk() { Ok(b) => b, Err(_) => return };
    let root = WindowHandle::from_raw(1).expect("root");

    // Top-level W at (100,100) 200x200, redirected to its own backing.
    let w = b.create_subwindow(None, root, 100, 100, 200, 200, 0,
        HostSubwindowVisual::Explicit { depth: 24, visual_xid: 0, colormap_xid: 0 }, None, None)
        .expect("top-level");
    let w_xid = w.as_raw();
    b.map_subwindow(None, w_xid).expect("map W");
    // Redirect W: allocate a backing pixmap and point W's redirected_target at it.
    let backing = /* create a 200x200 depth-24 pixmap; see create_pixmap usage in copy_plane test */;
    assert!(b.test_set_redirected_target(w_xid, backing.as_raw()));
    b.fill_rectangle(None, w_xid, 0x0000_0000, 0, 0, 200, 200).expect("clear W backing");

    // GC: Copy, IncludeInferiors, fg = red, width 1.
    b.apply_draw_state(None, &DrawState {
        subwindow_mode: SubwindowMode::IncludeInferiors,
        function: GcFunction::Copy,
        foreground: 0x00FF_0000,
        ..DrawState::default()
    }).expect("apply gc");

    // PolyRectangle on ROOT spanning W exactly: outline at screen (100,100)+200x200.
    let rect_bytes = pack_rectangles(&[(100i16, 100i16, 200u16, 200u16)]);
    b.poly_rectangle(None, root.as_raw(), 0x00FF_0000, &rect_bytes).expect("poly_rectangle");

    // Read W's backing (routed target): the rectangle's top edge is at W-local y=0.
    let px = b.get_image_pixels_for_tests(w_xid, 2, 0, 0, 200, 1, !0).expect("get_image").expect("bytes");
    let words: Vec<u32> = px.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
    assert!(words.iter().any(|&p| p & 0x00FF_FFFF == 0x00FF_0000),
        "stroke top edge must land in the redirected top-level's backing (IncludeInferiors)");
}
```

> Fill in `create_pixmap` for `backing` using the exact API the `copy_plane_depth1_extracts_mask_bits` test (line 439) uses to create pixmaps. Confirm `DrawState` field names (`function`, `foreground`, `subwindow_mode`, `line_width`) against `apply_draw_state`'s `DrawState` before writing.

- [ ] **Step 2: Run to verify it fails**

Run: `VK_ICD_FILENAMES=$MESA/lvp_icd.x86_64.json YSERVER_ALLOW_SOFTWARE_VULKAN=1 cargo test -p yserver --test render_acceptance stroke_on_root_include_inferiors_reaches -- --ignored --nocapture`
Expected: FAIL (before Task 2 was applied, W's backing is untouched). After Task 2, PASS. If implementing strictly TDD, stash Task 2 to observe the failure; otherwise verify it passes post-Task-2.

- [ ] **Step 3: Commit**

```bash
git add crates/yserver/tests/render_acceptance.rs
git commit -m "test(render): stroke on root IncludeInferiors reaches redirected backing (#90)"
```

---

### Task 4: Acceptance test — XOR single-pass has no double-invert gap (regression guard for the codex finding)

**Files:**
- Test: `crates/yserver/tests/render_acceptance.rs` (append).

Rationale: a draw+erase round-trip is symmetric even WITH double-invert, so it cannot catch the bug. The bug shows on a SINGLE draw: pixels where the outline crosses a non-redirected child of the redirected window get inverted twice → gap. Assert the outline is uniformly inverted after one pass.

- [ ] **Step 1: Write the test**

```rust
#[test]
#[ignore = "needs live Vulkan ICD"]
fn stroke_root_xor_include_inferiors_no_gap_over_subwindow() {
    use yserver_core::backend::WindowHandle;
    use yserver_core::host_x11::HostSubwindowVisual;
    let mut b = match KmsBackend::for_tests_with_vk() { Ok(b) => b, Err(_) => return };
    let root = WindowHandle::from_raw(1).expect("root");

    // Redirected top-level W (200x200 at 100,100) with a NON-redirected child C
    // placed so the rectangle's LEFT edge (screen x=100) crosses C.
    let w = b.create_subwindow(None, root, 100, 100, 200, 200, 0,
        HostSubwindowVisual::Explicit { depth: 24, visual_xid: 0, colormap_xid: 0 }, None, None).expect("W");
    let w_xid = w.as_raw();
    b.map_subwindow(None, w_xid).expect("map W");
    let c = b.create_subwindow(None, w, 0, 50, 40, 40, 0,
        HostSubwindowVisual::Explicit { depth: 24, visual_xid: 0, colormap_xid: 0 }, None, None).expect("C");
    b.map_subwindow(None, c.as_raw()).expect("map C");
    let backing = /* 200x200 depth-24 pixmap, as in Task 3 */;
    assert!(b.test_set_redirected_target(w_xid, backing.as_raw()));
    b.fill_rectangle(None, w_xid, 0x0000_0000, 0, 0, 200, 200).expect("clear W backing to 0");

    // GC: Invert, IncludeInferiors, width 1.
    b.apply_draw_state(None, &DrawState {
        subwindow_mode: SubwindowMode::IncludeInferiors,
        function: GcFunction::Invert,
        foreground: 0x00FF_FFFF,
        ..DrawState::default()
    }).expect("apply xor gc");

    // ONE PolyRectangle on root spanning W: left edge at screen x=100 (W-local x=0),
    // which runs down through C (W-local y=50..90).
    let rect_bytes = pack_rectangles(&[(100i16, 100i16, 200u16, 200u16)]);
    b.poly_rectangle(None, root.as_raw(), 0x00FF_FFFF, &rect_bytes).expect("poly_rectangle xor");

    // Left edge column in W's backing (W-local x=0) must be uniformly inverted
    // (0x00000000 -> 0x00FFFFFF) for the full height, including the C span.
    let col = b.get_image_pixels_for_tests(w_xid, 2, 0, 0, 1, 200, !0).expect("get_image").expect("bytes");
    let words: Vec<u32> = col.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
    for (y, &p) in words.iter().enumerate() {
        assert_eq!(p & 0x00FF_FFFF, 0x00FF_FFFF,
            "left-edge pixel at W-local y={y} must be inverted once (no double-invert gap over child C)");
    }
}
```

- [ ] **Step 2: Run → PASS** (with the Task 2 dedup). Confirm it would FAIL against a naive `collect_fill_rects_for_inferiors`-based implementation by temporarily swapping the method (optional sanity check, revert after).

Run: `VK_ICD_FILENAMES=$MESA/lvp_icd.x86_64.json YSERVER_ALLOW_SOFTWARE_VULKAN=1 cargo test -p yserver --test render_acceptance stroke_root_xor_include_inferiors_no_gap -- --ignored`

- [ ] **Step 3: Commit**

```bash
git add crates/yserver/tests/render_acceptance.rs
git commit -m "test(render): XOR IncludeInferiors stroke has no double-invert gap over subwindow (#90)"
```

---

### Task 5 (OPTIONAL — grab cursor crosshair): apply GrabPointer's cursor to the displayed sprite

**Files:**
- Modify: `crates/yserver-core/src/core_loop/process_request.rs` — `handle_grab_pointer` (23781), `handle_ungrab_pointer` (23968); disconnect path `process_disconnect.rs:340`.

`ActivePointerGrab.cursor` (set at process_request.rs:23924) has no readers; the displayed cursor is only set from a window's `define_cursor` attribute (process_request.rs:10675, 15955).

- [ ] **Step 1: Investigate the sprite-cursor override point.** Read `handle_grab_pointer` (23781–23966), the `define_cursor` call sites (10675, 15955), and how `cursor_host_xid` + `backend.define_cursor(origin, host_window, cursor_handle)` resolve. Determine how to force the displayed cursor to the grab cursor while a grab is active and how to revert on ungrab (re-resolve the window-under-pointer's cursor). Record the chosen mechanism in this step before implementing.
- [ ] **Step 2: Apply on grab** (in the success branch after `state.active_pointer_grab = Some(...)`, when `cursor.0 != 0`).
- [ ] **Step 3: Revert on ungrab** (`handle_ungrab_pointer` + disconnect path).
- [ ] **Step 4: Toolchain gates + commit.**

```bash
cargo +nightly fmt && cargo clippy --all-targets -- -D warnings && cargo test -p yserver --lib
git add crates/yserver-core/src/core_loop/process_request.rs
git commit -m "fix(input): apply GrabPointer cursor to the displayed sprite (#90)"
```

---

### Task 6: HW smoke on silence — REQUIRED before pushing (feedback_no_commit_before_smoke)

Static checks + lavapipe cannot confirm the visual result under a live compositor. The user observes first.

- [ ] **Step 1:** On silence (RX 580), in a yserver session with a compositing WM (cinnamon/xfce), run `import -silent /tmp/region.png` and drag a region crossing at least one window (e.g. a terminal).
- [ ] **Step 2:** Confirm the **selection rectangle is visible over the windows** during the drag with **no gaps** where it crosses window/sub-window edges, and erases cleanly as the drag resizes. If Task 5 was done, also confirm the **crosshair cursor** while the grab is held.
- [ ] **Step 3:** Confirm no stale-XOR residue after release, and a normal desktop (no import) is visually unchanged (no regression to root/desktop-background drawing or to fill-family IncludeInferiors, e.g. wallpaper/menus).
- [ ] **Step 4:** Only after the user confirms: `git push`.

---

## Self-Review

- **Spec coverage:** #90 symptom 1 (rectangle invisible) → Tasks 1–4, 6. Symptom 2 (crosshair) → Task 5. Codex round-1 findings: aliasing/double-invert → Task 2 dedup + Task 4 guard; weak test → Tasks 3–4 use redirected backing; `poly_point` → explicit Out-of-scope; clippy command → Task 2/5 use `cargo clippy --all-targets -- -D warnings`.
- **Type consistency:** `emit_stroke_output(host_xid, target, foreground, background, out)` and `collect_stroke_inferior_targets(host_xid, rects) -> Vec<(PaintTarget, Vec<Rectangle16>)>` used consistently across definition, call sites, and tests. `walk_stroke_inferiors` recursion signature matches its call.
- **Codex round-2: PASS.** Dedup algorithm verified sound (traced `root→W(→B)→C(→B)→G(→G_backing)`: `B` emitted once at `W`, skip-emit-but-recurse at `C`, `G_backing` emitted at `G` — no missed distinct backing, no double-cover); termination sound (single-parent window tree); child-local rect + `PaintTarget.offset` math correct (`fill_solid_rects` applies the offset); `get_image(w_xid)` reads the routed redirected backing so Tasks 3/4 exercise the compositor-visible path (`test_set_redirected_target` alone suffices). The one round-2 finding — inferiors collected from unclipped rects — is FIXED above (clip-first). No open correctness items remain; residual risk is the live-compositor behavior, covered by Task 6 HW smoke.
