# RENDER source-picture redirect resolution implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix the v2 KMS backend so that RENDER source pictures wrapping a redirected window resolve to the window's redirected backing pixmap (with the appropriate offset) instead of the window's leaf storage. Single user-visible target: **mate-panel notification-area systray icons render correctly** (long-standing invisibility, plus a 60 Hz damage/render loop driven by the same defect).

**Architecture:** The v2 backend already does ancestor-walk redirect routing on the destination side (`KmsBackendV2::resolve_paint_target`, with offset accumulation, called from every destination paint path). The source side is the gap: `resolve_picture_for_render` does `store.lookup(host_xid)` directly and returns the leaf `DrawableId` even when the picture's underlying drawable is a redirected window whose effective storage is its backing pixmap. This plan adds a backend method `resolve_source_picture` that routes `PictureRecord::Drawable` sources through `resolve_paint_target` and returns the resolved `(DrawableId, offset)`. For `render_composite`, the offset is plumbed into `CompositeRect.src_x` / `src_y` (and the matching mask coords). For `render_trapezoids` and `render_triangles_op`, the engine entrypoint has no explicit source-coordinate parameter; those paths swap to `resolve_source_picture` so the engine receives the correct backing `DrawableId`, and a readback test determines whether additional source-offset plumbing through the engine is needed or whether sampling is geometry-driven.

**Tech Stack:** Rust, X RENDER extension, KMS v2 backend (`crates/yserver/src/kms/v2/`), `cargo test`, `just yserver-mate-hw-trace` for bee/MATE smoke.

**Spec reference (Xorg as de-facto spec):**
- `xserver/render/picture.c:719` — picture default `subwindow-mode = ClipByChildren` (relevant only to the out-of-scope follow-up, but it's how Xorg avoids the loop-after-fix concern; see Risk register).
- `xserver/composite/compwindow.c:121-152` — `compSetPixmap` traversal making non-redirected descendants share their redirected ancestor's pixmap; v2's `resolve_paint_target` is the lazy equivalent.
- `xserver/render/picture.c` — `pPicture->pDrawable` for a redirected window points at the backing pixmap, so source sampling reads backing content naturally; v2's source path must mirror that.

**Current state (verified 2026-05-31 on `master` at commit e7199e4):**
- Destination side correct. `render_composite` (`crates/yserver/src/kms/v2/backend.rs:9887`) calls `resolve_dst_picture_for_render` then `resolve_paint_target`, applies `dst_target.offset` to `dst_x`/`dst_y` (`backend.rs:9945-9946`).
- Source side wrong. `render_composite:9872-9877` calls `resolve_picture_for_render` (`backend.rs:6174-6232`); for `PictureRecord::Drawable`, line 6193 does `store.lookup(*host_xid)` and returns the leaf `DrawableId`. Doc comment on the matching dst function at `backend.rs:6240-6244` literally says "callers feed `host_xid` through `KmsBackendV2::resolve_paint_target`"; the source function has no peer.
- `CompositeRect.src_x` / `src_y` are constructed at `backend.rs:9941-9942` with no offset added.
- `render_trapezoids` (call site ~line 10600) and `render_triangles_op` (call site ~line 10792) share the **source-resolution asymmetry** — both call `resolve_picture_for_render` and never route the source through redirect. They do **not** share the same coordinate-fix shape: their engine entrypoint `render_traps_or_tris` (called at lines 10664 and 10846) takes `src_resolved` plus trapezoid/triangle geometry bytes — there is no explicit `src_x` / `src_y` parameter to "apply an offset" to. The destination geometry is already pre-shifted by `dst_target.offset` (lines 10620-10635 for trapezoids). Whether the source sampling needs additional plumbing for descendant offsets depends on `render_traps_or_tris`'s internal sampling semantics; Phase 3 makes the resolve-call swap (which is well-defined) and lets a focused test decide whether further plumbing is needed.
- `render_composite_glyphs` (call site ~line 10116) also calls `resolve_picture_for_render`, but immediately matches `src_resolved` against `Solid`/`Gradient` only (line 10122–) and drops `ResolvedSource::Drawable(_)` — a redirected window-backed source picture cannot reach the glyph paint path, so this fix is irrelevant there and it is **not** included in Phase 3.
- Live diagnostic from a 2026-05-31 bee/MATE run (`yserver-hw-mate.log`): the tray icon proxy windows `0x2a00012` / `0x2a00014` (26×26) get ~525 damage events each via the `process_request.rs:1622` FillRectangles emit path. The applet writes the `Clear` to the proxy's backing (dst path is correct), but reads from `picture(proxy)` get an empty leaf source (this bug) → composite produces empty → icons invisible → loop continues.

**Why this fixes both symptoms with one defect:**
1. Icon client `PolyFillRect` on its window → `resolve_paint_target` walks to proxy → write lands in proxy's backing. ✓
2. Applet `Composite Over src=picture(proxy) → scratch` → current bug: source resolves to proxy's leaf → samples empty. → tray icon invisible.
3. Applet `FillRectangles op=Clear` on `picture(proxy)` → dst correctly routes to backing → wipes the icon pixels (legitimate write, legitimate damage). DAMAGE-Notify fires on the real write → applet wakes → reads empty leaf again → loop.

Fix the source path → applet's composite reads the backing (icon visible). Step 3's Clear-wipe is still a concern (see Risk register), but is a separate bug.

---

## File structure

**Modified (all in `crates/yserver/src/kms/v2/backend.rs`):**

- Add new method `KmsBackendV2::resolve_source_picture` (placed adjacent to `resolve_paint_target` ~line 1333). Returns `(ResolvedSource, Repeat, Option<PictTransform>, bool, (i32, i32))` — the existing four fields plus an `offset` for source coordinate plumbing.
- Update RENDER call sites:
  - `render_composite` (~line 9852): switches to `self.resolve_source_picture(...)` and applies the returned offset to `CompositeRect.src_x` / `src_y` / `mask_x` / `mask_y`. Client-clip translation handled separately by Task 2.3.
  - `render_trapezoids` (call site ~line 10600) and `render_triangles_op` (~line 10792): switch to `self.resolve_source_picture(...)` so the source resolves to the backing `DrawableId`. The engine entrypoint (`render_traps_or_tris`, lines 10664 / 10846) has no `src_x` / `src_y` parameter; any descendant-offset plumbing on the source side is decided by a readback test in Phase 3 rather than assumed.
- `render_composite_glyphs` excluded — see "Current state" above.
- `resolve_picture_for_render` (~line 6174) stays as-is — it remains the protocol-resolution primitive; the new method wraps it (or duplicates the `Drawable` branch, see Task 1.1 for the concrete shape).

**New tests (added inline to existing modules; no new files):**

- `backend.rs` mod tests: `resolve_source_picture` returns backing + offset for redirected drawable; returns identity for non-redirected; returns offset (0, 0) for Solid / Gradient sources.
- `backend.rs` engine-interaction tests: `render_composite` issues a draw with translated `src_x` / `src_y` when the source picture wraps a redirected descendant.
- `backend.rs` integration: tray-shape end-to-end test confirming a child write + applet-style composite-from-proxy-picture surfaces the child's pixel in the dst.

**Not modified:**

- `crates/yserver-core/src/core_loop/process_request.rs` — no core-side changes; the bug is entirely in v2 backend.
- `crates/yserver-core/src/core_loop/damage_fanout.rs` — existing ancestor walk is correct.
- v1 (`host_x11`) backend — irrelevant; bee/MATE runs on v2.

---

## Phase 1: `resolve_source_picture` helper

### Task 1.1: Failing test — drawable picture on a redirected window returns backing + zero offset

**Files:**
- Modify: `crates/yserver/src/kms/v2/backend.rs` tests module (locate via `grep -n "fn resolve_paint_target_redirected_window_routes_to_backing" crates/yserver/src/kms/v2/backend.rs` — line 15490; place new tests near there).

- [ ] **Step 1: Locate an existing `PictureRecord::Drawable` fixture to copy from**

Run:

```
grep -nB1 -A12 "PictureRecord::Drawable {" crates/yserver/src/kms/v2/backend.rs | head -40
```

Pick the most-recent in-test instantiation (search `#\[cfg(test)\]` / `#[test]` proximity in the result). Copy the **exact field set** verbatim into the new test — do not invent fields. The struct definition lives in `KmsCore` / `core_loop` types; field names like `clip_x`, `clip_y`, `client_clip`, `pict_format` may not all be present in the current shape.

- [ ] **Step 2: Add the failing test, replacing the `<copy-fixture>` placeholder**

```rust
#[test]
fn resolve_source_picture_redirected_window_routes_to_backing() {
    use crate::kms::v2::store::{DrawableKind, Storage};
    let mut b = KmsBackendV2::for_tests();
    // Window 0x100 at root, redirected to backing 0x900.
    let w_id = seed_window(&mut b, 0x100, None, 0, 0);
    let backing_id = b
        .store
        .allocate(
            0x900,
            DrawableKind::RedirectedBacking,
            32,
            false,
            Storage::for_tests_null(
                ash::vk::Extent2D { width: 100, height: 100 },
                ash::vk::Format::B8G8R8A8_UNORM,
            ),
        )
        .expect("backing allocate");
    b.store.set_redirected_target(w_id, Some(backing_id));
    // Register a Drawable picture wrapping the window's host xid.
    // <copy-fixture: paste the PictureRecord::Drawable initializer from
    // an existing test (Step 1's grep), substituting host_xid: 0x100,
    // repeat: Repeat::None, transform: None, component_alpha: false>.
    b.core.pictures.insert(0xA000, /* fixture */);

    let (resolved, repeat, transform, ca, offset) =
        b.resolve_source_picture(0xA000).expect("resolve");
    assert!(matches!(
        resolved,
        crate::kms::v2::engine::ResolvedSource::Drawable(id) if id == backing_id
    ));
    assert!(matches!(repeat, crate::kms::cpu_types::Repeat::None));
    assert_eq!(transform, None);
    assert_eq!(ca, false);
    assert_eq!(offset, (0, 0));
}
```

- [ ] **Step 3: Run to confirm failure**

Run: `cargo test -p yserver --lib resolve_source_picture_redirected_window_routes_to_backing`

Expected: FAIL with `no method named 'resolve_source_picture' on KmsBackendV2`.

- [ ] **Step 4: Implement `resolve_source_picture`**

Place the method on `impl KmsBackendV2` adjacent to `resolve_paint_target` (after line 1366; pick a stable insertion point inside the same `impl` block):

```rust
/// Source-picture analogue of `resolve_paint_target`. Returns the
/// `ResolvedSource` already walked through COMPOSITE redirect: for
/// a `PictureRecord::Drawable` whose `host_xid` identifies a
/// window with an effective redirected ancestor backing, the
/// `Drawable(id)` is the BACKING's id and the returned offset is
/// the descendant's offset within the backing. Callers must add
/// that offset to the picture's `src_x` / `src_y` before passing
/// the rect to the engine, so the sample position lands on the
/// right sub-region of the backing.
///
/// For `Solid` / `Gradient` / `None` picture variants, no redirect
/// walk applies and the offset is `(0, 0)`.
///
/// Mirrors Xorg's `pPicture->pDrawable` redirection: in Xorg a
/// redirected window's `pDrawable` points at the backing pixmap
/// (`xserver/composite/compwindow.c:121-152`), so the picture
/// already samples backing content. v2 retains the leaf `host_xid`
/// in `PictureRecord::Drawable`, so the walk happens at sample
/// time via this helper.
pub(crate) fn resolve_source_picture(
    &self,
    host_pic: u32,
) -> Option<(
    crate::kms::v2::engine::ResolvedSource,
    crate::kms::cpu_types::Repeat,
    Option<PictTransform>,
    bool,
    (i32, i32),
)> {
    use crate::kms::v2::engine::ResolvedSource;
    let record = self.core.pictures.get(&host_pic)?;
    match record {
        PictureRecord::Drawable {
            host_xid,
            repeat,
            transform,
            component_alpha,
            ..
        } => {
            let target = self.resolve_paint_target(*host_xid)?;
            Some((
                ResolvedSource::Drawable(target.id),
                *repeat,
                *transform,
                *component_alpha,
                target.offset,
            ))
        }
        _ => {
            // Non-Drawable picture variants — fall through to the
            // existing resolution path; offset = (0, 0).
            let (resolved, repeat, transform, ca) =
                resolve_picture_for_render(&self.core, &self.store, host_pic)?;
            Some((resolved, repeat, transform, ca, (0, 0)))
        }
    }
}
```

> Note: `PictTransform` is the type currently used in `resolve_picture_for_render`'s return tuple — same import path. If the path resolution is finicky, copy verbatim from the existing function's signature.

- [ ] **Step 5: Run to confirm pass**

Run: `cargo test -p yserver --lib resolve_source_picture_redirected_window_routes_to_backing`

Expected: PASS.

- [ ] **Step 6: Commit**

```
git add crates/yserver/src/kms/v2/backend.rs
git commit -m "feat(render): add resolve_source_picture to walk redirect for source pictures

Source picture resolution previously bypassed redirect routing — a
picture wrapping a redirected window resolved to the leaf drawable,
not the backing. This caused the systray applet's Composite Over
src=picture(proxy) to read empty leaf content (icons invisible) and
its FillRect Clear on the same picture to damage the backing every
frame (60 Hz loop). New helper mirrors resolve_paint_target's
ancestor walk and returns (ResolvedSource, offset) so callers can
translate src_x/src_y before sampling."
```

### Task 1.2: Failing test — descendant of redirected ancestor returns backing + accumulated offset

**Files:**
- Modify: `crates/yserver/src/kms/v2/backend.rs` tests module

- [ ] **Step 1: Add the test**

```rust
#[test]
fn resolve_source_picture_descendant_picks_up_ancestor_backing_with_offset() {
    use crate::kms::v2::store::{DrawableKind, Storage};
    let mut b = KmsBackendV2::for_tests();
    // Outer 0x100 redirected to backing 0x900; child 0x200 at (10, 20)
    // under outer; grandchild 0x300 at (3, 4) under child.
    let w_id = seed_window(&mut b, 0x100, None, 0, 0);
    let _c_id = seed_window(&mut b, 0x200, Some(0x100), 10, 20);
    let _g_id = seed_window(&mut b, 0x300, Some(0x200), 3, 4);
    let backing_id = b
        .store
        .allocate(
            0x900,
            DrawableKind::RedirectedBacking,
            32,
            false,
            Storage::for_tests_null(
                ash::vk::Extent2D { width: 200, height: 200 },
                ash::vk::Format::B8G8R8A8_UNORM,
            ),
        )
        .expect("backing allocate");
    b.store.set_redirected_target(w_id, Some(backing_id));
    // Picture wraps the grandchild's host xid. <copy-fixture: paste
    // the PictureRecord::Drawable initializer from an existing test
    // and substitute host_xid: 0x300>.
    b.core.pictures.insert(0xA000, /* fixture w/ host_xid=0x300 */);

    let (resolved, _, _, _, offset) =
        b.resolve_source_picture(0xA000).expect("resolve");
    assert!(matches!(
        resolved,
        crate::kms::v2::engine::ResolvedSource::Drawable(id) if id == backing_id
    ));
    assert_eq!(offset, (13, 24));
}
```

- [ ] **Step 2: Run to verify pass**

Run: `cargo test -p yserver --lib resolve_source_picture_descendant_picks_up_ancestor_backing_with_offset`

Expected: PASS (`resolve_paint_target` already handles the ancestor walk; this just exercises the wrap).

- [ ] **Step 3: Commit**

```
git add crates/yserver/src/kms/v2/backend.rs
git commit -m "test(render): cover resolve_source_picture descendant offset accumulation"
```

### Task 1.3: Failing test — non-redirected drawable picture returns identity

**Files:**
- Modify: `crates/yserver/src/kms/v2/backend.rs` tests module

- [ ] **Step 1: Add the test**

```rust
#[test]
fn resolve_source_picture_unredirected_drawable_returns_identity() {
    let mut b = KmsBackendV2::for_tests();
    let _w_id = seed_window(&mut b, 0x100, None, 0, 0);
    // <copy-fixture: PictureRecord::Drawable with host_xid: 0x100>.
    b.core.pictures.insert(0xA000, /* fixture w/ host_xid=0x100 */);
    let (_, _, _, _, offset) = b.resolve_source_picture(0xA000).expect("resolve");
    assert_eq!(offset, (0, 0));
}
```

- [ ] **Step 2: Run to verify pass**

Run: `cargo test -p yserver --lib resolve_source_picture_unredirected_drawable_returns_identity`

Expected: PASS.

- [ ] **Step 3: Commit**

```
git add crates/yserver/src/kms/v2/backend.rs
git commit -m "test(render): cover resolve_source_picture identity for unredirected drawables"
```

### Task 1.4: Failing test — Solid / Gradient picture variants return offset (0, 0)

**Files:**
- Modify: `crates/yserver/src/kms/v2/backend.rs` tests module

- [ ] **Step 1: Add the test**

```rust
#[test]
fn resolve_source_picture_solid_returns_zero_offset() {
    let mut b = KmsBackendV2::for_tests();
    b.core.pictures.insert(
        0xA000,
        PictureRecord::SolidFill {
            premul: [1.0, 0.5, 0.25, 1.0],
            repeat: crate::kms::cpu_types::Repeat::None,
            component_alpha: false,
        },
    );
    let (resolved, _, _, _, offset) = b.resolve_source_picture(0xA000).expect("resolve");
    assert!(matches!(
        resolved,
        crate::kms::v2::engine::ResolvedSource::Solid(_)
    ));
    assert_eq!(offset, (0, 0));
}
```

- [ ] **Step 2: Run to verify pass**

Run: `cargo test -p yserver --lib resolve_source_picture_solid_returns_zero_offset`

Expected: PASS.

- [ ] **Step 3: Commit**

```
git add crates/yserver/src/kms/v2/backend.rs
git commit -m "test(render): cover resolve_source_picture identity for non-Drawable sources"
```

---

## Phase 2: `render_composite` source + mask paths

### Task 2.1: Failing test — `render_composite` with redirected source translates `src_x` / `src_y`

**Files:**
- Modify: `crates/yserver/src/kms/v2/backend.rs` tests module

Identify the recording-style hook the existing tests use for engine introspection: search for `engine.last_composite_rect` / `last_render_call` patterns with `grep -n "fn render_composite_records_rect\|last_composite_rect\|RecordedComposite" crates/yserver/src/kms/v2/`. The v2 engine has a test surface for inspecting issued composite calls; reuse it. If the existing harness doesn't expose `src_x` / `src_y` on the recorded call, extend it as a one-line getter — match the field shape of `CompositeRect` (`backend.rs:9940-9949`).

- [ ] **Step 1: Add the failing test**

```rust
#[test]
fn render_composite_src_picture_on_redirected_window_translates_sample_coords() {
    use crate::kms::v2::store::{DrawableKind, Storage};
    let mut b = KmsBackendV2::for_tests_with_engine_recorder();
    // Outer 0x100 → backing 0x900. Child 0x200 at (10, 20) under outer.
    let w_id = seed_window(&mut b, 0x100, None, 0, 0);
    let _c_id = seed_window(&mut b, 0x200, Some(0x100), 10, 20);
    let backing_id = b
        .store
        .allocate(
            0x900,
            DrawableKind::RedirectedBacking,
            32,
            false,
            Storage::for_tests_null(
                ash::vk::Extent2D { width: 100, height: 100 },
                ash::vk::Format::B8G8R8A8_UNORM,
            ),
        )
        .expect("backing allocate");
    b.store.set_redirected_target(w_id, Some(backing_id));

    // Src picture wraps the child (descendant of redirected outer).
    // <copy-fixture: use the PictureRecord::Drawable initializer from
    // an existing test (grep -nB1 -A12 "PictureRecord::Drawable {"
    // crates/yserver/src/kms/v2/backend.rs); substitute host_xid: 0x200>.
    b.core.pictures.insert(0xA000, /* fixture w/ host_xid=0x200 */);
    // Dst picture wraps an unrelated pixmap so the dst path is identity.
    let dst_pix_id = b
        .store
        .allocate(
            0xB000,
            DrawableKind::Pixmap,
            32,
            false,
            Storage::for_tests_null(
                ash::vk::Extent2D { width: 100, height: 100 },
                ash::vk::Format::B8G8R8A8_UNORM,
            ),
        )
        .expect("dst pixmap allocate");
    // <copy-fixture: same shape, substitute host_xid: 0xB000>.
    b.core.pictures.insert(0xA001, /* fixture w/ host_xid=0xB000 */);

    // Composite Over: read src at (3, 4), write dst at (0, 0), 5×6.
    let _ = b.render_composite(
        None,
        /*op=Over*/ 3,
        /*host_src*/ 0xA000,
        /*host_mask*/ 0,
        /*host_dst*/ 0xA001,
        /*src_x*/ 3,
        /*src_y*/ 4,
        /*mask_x*/ 0,
        /*mask_y*/ 0,
        /*dst_x*/ 0,
        /*dst_y*/ 0,
        /*width*/ 5,
        /*height*/ 6,
    );

    let recorded = b.engine_recorder_last_composite().expect("call recorded");
    // Source must sample from the BACKING with the child's offset
    // applied: src_xy = (3 + 10, 4 + 20) = (13, 24).
    assert_eq!(recorded.src_id, backing_id);
    assert_eq!(recorded.src_x, 13);
    assert_eq!(recorded.src_y, 24);
    // Dst unchanged (pixmap, no redirect).
    assert_eq!(recorded.dst_id, dst_pix_id);
    assert_eq!(recorded.dst_x, 0);
    assert_eq!(recorded.dst_y, 0);
}
```

> The `for_tests_with_engine_recorder` and `engine_recorder_last_composite` helpers are placeholders for the existing engine-call test surface — name them whatever the current pattern uses (e.g. `engine.last_composite_call()`). Audit the existing tests at `backend.rs:13661` (`poly_fill_rectangle_honours_gc_clip`) and around line 14000+ for the active idiom.

- [ ] **Step 2: Run to confirm failure**

Run: `cargo test -p yserver --lib render_composite_src_picture_on_redirected_window_translates_sample_coords`

Expected: FAIL — current `render_composite` doesn't translate source coords.

- [ ] **Step 3: Update `render_composite` to use the new helper**

In `render_composite` (~line 9852), replace lines 9872-9886 (the source + mask resolve block) with:

```rust
let Some((src_resolved, src_repeat, src_transform, _src_ca, src_offset)) =
    self.resolve_source_picture(host_src)
else {
    log::debug!("v2 render_composite gap: host_src 0x{host_src:x} not resolvable");
    return Ok(());
};
let (mask_resolved, mask_repeat, mask_transform, mask_component_alpha, mask_offset) =
    if host_mask == 0 {
        (
            ResolvedSource::None,
            Repeat::None,
            None,
            false,
            (0_i32, 0_i32),
        )
    } else {
        let Some(t) = self.resolve_source_picture(host_mask) else {
            log::debug!(
                "v2 render_composite gap: host_mask 0x{host_mask:x} not resolvable"
            );
            return Ok(());
        };
        t
    };
```

Then update the `CompositeRect` construction (lines 9940-9949) to apply the source / mask offsets — **only the sample coordinates fed to the engine**:

```rust
let rect = crate::kms::vk::ops::render::CompositeRect {
    src_x: i32::from(src_x) + src_offset.0,
    src_y: i32::from(src_y) + src_offset.1,
    mask_x: i32::from(mask_x) + mask_offset.0,
    mask_y: i32::from(mask_y) + mask_offset.1,
    dst_x: i32::from(dst_x) + dst_target.offset.0,
    dst_y: i32::from(dst_y) + dst_target.offset.1,
    width: u32::from(width),
    height: u32::from(height),
};
```

**Leave `src_translation` / `mask_translation` (~lines 9924-9931) alone** in this task. The picture-client-clip translation path may or may not need the offset folded in — Task 2.3 below tests that specifically. Untested offset changes to client-clip math can paint into the wrong region on otherwise-working clients (xfwm4 / muffin shadow blits exercise these clip paths heavily).

- [ ] **Step 4: Run to confirm pass**

Run: `cargo test -p yserver --lib render_composite_src_picture_on_redirected_window_translates_sample_coords`

Expected: PASS.

- [ ] **Step 5: Run the full render_composite test family for regressions**

Run: `cargo test -p yserver --lib render_composite`

Expected: All PASS.

- [ ] **Step 6: Commit**

```
git add crates/yserver/src/kms/v2/backend.rs
git commit -m "fix(render): walk redirect for render_composite source + mask pictures

When a source/mask picture wraps a redirected window, sample
coordinates must translate by the descendant→ancestor-backing
offset so the engine reads the right sub-region of the backing.
Before this fix, picture(redirected_window) sampled the leaf
storage (empty under Manual redirect) and the systray applet's
Composite Over from picture(proxy) produced blank output —
icons invisible."
```

### Task 2.2: Failing test — mask picture on a redirected window translates `mask_x` / `mask_y`

**Files:**
- Modify: `crates/yserver/src/kms/v2/backend.rs` tests module

- [ ] **Step 1: Add the test**

Mirror Task 2.1 but use the redirected-descendant picture as the mask (host_mask = 0xA000) and a pixmap-backed source. Assert `recorded.mask_id == backing_id`, `recorded.mask_x == 3 + 10`, `recorded.mask_y == 4 + 20`.

```rust
#[test]
fn render_composite_mask_picture_on_redirected_window_translates_sample_coords() {
    // ... same setup as Task 2.1, swap roles: 0xA000 is mask, fresh pixmap is src ...
}
```

- [ ] **Step 2: Run to confirm pass (should already pass after Task 2.1's mask offset wiring)**

Run: `cargo test -p yserver --lib render_composite_mask_picture_on_redirected_window_translates_sample_coords`

Expected: PASS.

- [ ] **Step 3: Commit**

```
git add crates/yserver/src/kms/v2/backend.rs
git commit -m "test(render): cover mask-side redirect translation for render_composite"
```

### Task 2.3: Client-clip translation audit for redirected sources

When a source picture has a client-set clip (`SetPictureClipRectangles`), `render_composite` folds it into the composite region via `src_translation = (dst_origin - src_origin)` (~`backend.rs:9924-9931`). After Task 2.1 the sample coordinates account for the source offset, but the existing `src_translation` calculation still uses the un-translated `src_x` / `src_y`. The question this task answers: **does that mismatch cause a real regression for clients that set source-picture client clips on a redirected window?**

This is the case codex flagged as easy-to-get-wrong. The fix may be one of:
- No change — the existing `src_translation` math happens to remain correct because client_clip is in picture-local coords, and the translation maps picture-local-to-picture-local.
- Fold source offset in — required if backing-coord math is needed end-to-end.

Decide via a test that actually exercises a client-set clip on a redirected source.

**Files:**
- Modify: `crates/yserver/src/kms/v2/backend.rs` tests module

- [ ] **Step 1: Write the failing test (or passing — outcome confirms which branch)**

```rust
#[test]
fn render_composite_src_picture_client_clip_on_redirected_window_clips_correctly() {
    // Setup: same redirect topology as Task 2.1 (outer 0x100 → backing
    // 0x900; child 0x200 at (10, 20)). Source picture wraps the child.
    // Apply a source-picture client clip of {x=2, y=3, w=2, h=2} (in
    // picture-local coords on the child). Issue a composite Over from
    // src at (0, 0) into a dst pixmap at (0, 0), 5×5. Read back the
    // dst: only the (2, 3)-(4, 5) sub-region should have been touched
    // (writes outside the clip must be skipped).
    //
    // Setup details: copy the picture initializer from an existing
    // SetPictureClipRectangles test — `grep -nE "client_clip.*Some|fn .*client_clip" crates/yserver/src/kms/v2/backend.rs`.
}
```

- [ ] **Step 2: Run and observe**

Run: `cargo test -p yserver --lib render_composite_src_picture_client_clip_on_redirected_window_clips_correctly`

**Branch on the outcome:**

- **PASS** → existing `src_translation` math is correct for redirected sources too. Commit the test as a confirmation-of-correctness regression and skip Step 3.
- **FAIL** → the clip translation is off by the source's offset. Proceed to Step 3.

- [ ] **Step 3 (conditional, only if Step 2 failed): Fold source offset into `src_translation` / `mask_translation`**

Update lines 9924-9931 of `render_composite` to:

```rust
let src_translation = (
    dst_origin_x - (i32::from(src_x) + src_offset.0),
    dst_origin_y - (i32::from(src_y) + src_offset.1),
);
let mask_translation = (
    dst_origin_x - (i32::from(mask_x) + mask_offset.0),
    dst_origin_y - (i32::from(mask_y) + mask_offset.1),
);
```

- [ ] **Step 4: Re-run the test and the broader clip regression set**

```
cargo test -p yserver --lib render_composite_src_picture_client_clip_on_redirected_window_clips_correctly
cargo test -p yserver --lib client_clip
cargo test -p yserver --lib composite_clip
```

Expected: all PASS.

- [ ] **Step 5: Commit**

If Step 3 ran:

```
git add crates/yserver/src/kms/v2/backend.rs
git commit -m "fix(render): fold source offset into src_translation for redirected pictures

Source-picture client clips were getting translated through
src_origin in picture-local coords while the source storage was
already shifted by the redirect offset. Picked up by a redirected-
source-with-client-clip composite test."
```

If Step 3 didn't run (test passed without the fold-in):

```
git add crates/yserver/src/kms/v2/backend.rs
git commit -m "test(render): confirm src client-clip translation correct for redirected sources"
```

---

## Phase 3: Other RENDER call sites

`render_trapezoids` and `render_triangles_op` both call `resolve_picture_for_render` for their source picture and never walk the redirect. Switch the call to `self.resolve_source_picture` so the resolved `ResolvedSource::Drawable(...)` carries the backing id when the source wraps a redirected window. That is the half of the fix that's identical to Phase 2.

The other half is **NOT** identical: the engine entrypoint `render_traps_or_tris` (line 10664 for trapezoids; line 10846 for triangles) takes `(src_resolved, dst_target.id, ...)` plus primitive geometry — there is no `src_x` / `src_y` parameter to translate. The destination geometry is already pre-shifted by `dst_target.offset` in the surrounding code (e.g. lines 10620-10635 for trapezoids). Whether the source side needs additional handling for descendant offsets (e.g. a `src_transform` translation matrix, a separate source-origin field threaded to the engine, or no change at all because trapezoid sampling correspondence is geometry-driven rather than offset-driven) depends on `render_traps_or_tris`'s sampling semantics.

Phase 3's two source-resolve tasks therefore prescribe **only** the resolve-call swap, with a test that asserts the engine receives `ResolvedSource::Drawable(backing_id)` (proving the helper switch took effect). Each task ends with a focused interrogation step that exercises a redirected source through the trap/tri engine with a descendant offset and inspects the output — if the output is wrong, the fix shape gets designed at that point against the engine code; if it's correct, no further work needed.

`render_composite_glyphs` is **excluded** from this phase: it rejects `ResolvedSource::Drawable(_)` at its post-resolve `match` (`backend.rs:10122–`), accepting only `Solid` / `Gradient` sources. A redirected window-backed picture cannot route through that op, so applying the fix there would be churn without exercising the defect.

### Task 3.1: `render_trapezoids` source — resolve through redirect

**Files:**
- Modify: `crates/yserver/src/kms/v2/backend.rs` — the function containing the `resolve_picture_for_render` call site at line 10600.

- [ ] **Step 1: Read `render_traps_or_tris` to learn how it samples the source**

```
grep -nB2 -A40 "fn render_traps_or_tris" crates/yserver/src/kms/v2/engine.rs crates/yserver/src/kms/v2/engine/*.rs
```

Skim the engine method's first ~80 lines. Note where `src_resolved` is used (a sampler binding? a copy-into-staging? a sample coordinate transform?). Record findings in your scratch notes for Step 5 below.

- [ ] **Step 2: Failing test — engine receives the backing's `DrawableId` when source wraps a redirected window**

```rust
#[test]
fn render_trapezoids_src_picture_on_redirected_window_resolves_to_backing() {
    // Setup mirrors Task 2.1's redirect topology: outer 0x100 → backing 0x900;
    // child 0x200 at (10, 20). Source picture wraps the child.
    // Issue a render_trapezoids call with a single trapezoid; assert the
    // engine receives `ResolvedSource::Drawable(backing_id)`.
    //
    // Use the same engine-recorder pattern Task 2.1 set up; trap calls
    // record the `src_resolved` argument. If the recorder doesn't yet
    // capture trap-or-tris calls, extend it (one-line getter mirroring
    // composite's surface).
    // <copy-fixture: PictureRecord::Drawable initializer; build a
    // trapezoid_body helper similar to render_composite test bodies.>
}
```

- [ ] **Step 3: Run to confirm failure**

Run: `cargo test -p yserver --lib render_trapezoids_src_picture_on_redirected_window_resolves_to_backing`

Expected: FAIL — the current `resolve_picture_for_render` returns the leaf id, not the backing.

- [ ] **Step 4: Switch source resolve to `self.resolve_source_picture`**

In the function at line 10600, change:

```rust
let Some((src_resolved, src_repeat, src_transform, _src_ca)) =
    resolve_picture_for_render(&self.core, &self.store, host_src)
else { ... };
```

to:

```rust
let Some((src_resolved, src_repeat, src_transform, _src_ca, _src_offset)) =
    self.resolve_source_picture(host_src)
else { ... };
```

`_src_offset` is captured but left unused for now — Step 5 decides whether it needs plumbing through to the engine.

- [ ] **Step 5: Run the test from Step 3 + check whether descendant offsets need additional handling**

Run: `cargo test -p yserver --lib render_trapezoids_src_picture_on_redirected_window_resolves_to_backing`

Expected: PASS — the engine now receives the backing's id.

Now write a **second** test that exercises the same setup but actually renders into a dst pixmap and reads back the result. Use the same engine-readback helper as Task 4.1 (`grep -nE "fn readback|engine_readback|fn read_dst" crates/yserver/src/kms/v2/engine`). Set up a known pixel pattern in the backing at the descendant's offset region (e.g. write `0xFFRRGGBB` to backing-local (13, 24) sized 3×3 — see Task 4.1 for the pattern), then issue a trapezoid that covers dst-local (0, 0)–(3, 3). Assert that the dst captures the same RGB.

Two outcomes:

- **PASS** — `render_traps_or_tris` samples the source at trap-geometry-aligned coords, and since the destination geometry was already shifted by `dst_target.offset` (the trapezoid lands at the right backing position on the dst), the source naturally samples from the correct backing region. No further plumbing needed. Add the readback test as a regression and proceed to Step 6.

- **FAIL** — the source samples from the wrong backing region. Read the engine's sampling code (Step 1 notes) to find the right plumbing seam (likely either prepending a translation to `src_transform`, or adding a `src_offset` parameter to `render_traps_or_tris`). Implement, re-run the test, then proceed to Step 6.

- [ ] **Step 6: Run the trapezoids regression family**

Run: `cargo test -p yserver --lib trapezoid`

Expected: All PASS.

- [ ] **Step 7: Commit**

If Step 5 needed extra plumbing, commit with a message that describes the plumbing seam taken (e.g. `fix(render): thread src_offset through render_traps_or_tris for descendant sources`). Otherwise:

```
git add crates/yserver/src/kms/v2/backend.rs
git commit -m "fix(render): route render_trapezoids source picture through redirect

The src picture resolve now uses resolve_source_picture so a
picture wrapping a redirected window resolves to the backing's
DrawableId. Sampling correspondence is geometry-driven in the
engine's render_traps_or_tris path; no additional source-offset
plumbing needed — verified by a readback test against a known
descendant pixel pattern in the backing."
```

### Task 3.2: `render_triangles_op` source — resolve through redirect

Apply Task 3.1's seven-step pattern to the function containing the `resolve_picture_for_render` call site at line 10792 (engine entrypoint at line 10846).

The same uncertainty about source-offset plumbing applies — Step 5's readback test is what determines whether `render_triangles_op` needs additional plumbing beyond the resolve-call swap.

- [ ] **Step 1: Read `render_traps_or_tris` triangle path semantics — likely the same code as trapezoids; check whether the triangle primitive kind alters source sampling**
- [ ] **Step 2: Failing test — engine receives backing id (mirror Task 3.1 Step 2 with `TrapPrimKind::Triangle`)**
- [ ] **Step 3: Confirm failure**
- [ ] **Step 4: Switch source resolve to `self.resolve_source_picture`**
- [ ] **Step 5: Readback test + decide on offset plumbing**
- [ ] **Step 6: `cargo test -p yserver --lib triangle` for the regression family**
- [ ] **Step 7: Commit, naming the plumbing seam if one was taken**

### Task 3.3: Audit-only sweep

After Tasks 3.1 and 3.2, run the audit grep once more to make sure no `resolve_picture_for_render` call site that handles a `Drawable` source bypasses redirect:

```
grep -n "resolve_picture_for_render(" crates/yserver/src/kms/v2/backend.rs
```

- [ ] **Step 1: Inspect every remaining call site**

Expected remaining call sites:
- `resolve_picture_for_render` itself (the free function — fine, still used by `resolve_source_picture`'s non-Drawable fallback).
- `render_composite_glyphs` (~line 10116) — fine, that op only handles `Solid` / `Gradient` sources (see Phase 3 intro); a redirected drawable picture can't reach it.
- Calls inside `tests::` modules — fine, exercising the protocol primitive directly.

Anything else is a missed conversion — apply Task 3.1's pattern and add a test.

- [ ] **Step 2: If any call sites surface, commit per-call-site fixes**

---

## Phase 4: End-to-end regression — tray-shape scenario

### Task 4.1: Full integration test

**Files:**
- Modify: `crates/yserver/src/kms/v2/backend.rs` tests module

- [ ] **Step 1: Add the failing test**

```rust
#[test]
fn tray_pattern_composite_from_redirected_proxy_reads_child_pixels() {
    // Stage the systray-applet pattern:
    //   1. Proxy window 0x100 (24×27) at root, manually redirected to backing 0x900.
    //   2. Embedded icon window 0x200 (24×27) reparented as child of 0x100 at (0, 0).
    //   3. Icon-client write at (5, 5) sized (3, 3) — RGBA solid 0xFFRRGGBB.
    //   4. Applet composite from picture(proxy) at src_xy=(0,0) into a scratch dst pixmap
    //      at dst_xy=(0,0) sized 24×27.
    //   5. Read back from the scratch pixmap — the (5, 5)–(8, 8) pixels must
    //      be non-empty.
    //
    // Without the redirect-source fix this composite reads the proxy's
    // leaf (empty) and the dst stays cleared; with the fix it reads the
    // backing and the icon contribution surfaces.
    // Implementation note: use whatever readback helper the v2 engine
    // tests already provide (`engine.readback_dst(...)` or similar);
    // grep `crates/yserver/src/kms/v2/engine` for usage.
    /* concrete test body — fill in following the engine-readback
       pattern used in existing render-engine tests. The structure
       follows Task 2.1's setup verbatim, then issues a
       `poly_fill_rectangle` against the child (writing to backing)
       and a `render_composite` from picture(proxy) into the
       scratch dst, then asserts the scratch pixel at (5, 5) is
       non-zero. */
}
```

> This is the load-bearing regression — if it passes, the systray-icons-invisible bug is fixed. The placeholder comment is intentional because the readback helper varies by Vk vs CPU engine; the executor must pick the matching one and follow the existing pattern. Search `grep -nE "fn readback|engine_readback|fn read_dst" crates/yserver/src/kms/v2/engine` to identify it.

- [ ] **Step 2: Run to confirm failure (pre-Phase-2 fix) — should already pass after Phase 2 lands**

Run: `cargo test -p yserver --lib tray_pattern_composite_from_redirected_proxy_reads_child_pixels`

Expected: PASS (Phase 2 already fixed the load-bearing path).

If this test FAILS after Phases 1–3, that means a path the systray exercises still bypasses redirect — bisect by grep'ing `resolve_picture_for_render` and checking each remaining caller.

- [ ] **Step 3: Commit**

```
git add crates/yserver/src/kms/v2/backend.rs
git commit -m "test(render): tray-shape integration — composite from picture(redirected proxy)"
```

---

## Phase 5: Hardware smoke

These steps run on bee under MATE; user-driven per the project's hw-recipe convention.

### Task 5.1: Capture a fresh trace + log

- [ ] **Step 1: User runs**

```
just yserver-mate-hw-trace
```

Let the session idle ~15 s with mate-panel + notification area visible. Then close the session.

Expected artifacts: `mate.xtrace`, `yserver-hw-mate.log`.

### Task 5.2: Confirm icons render

- [ ] **Step 1: Eyeball the panel**

Expected (load-bearing):
- Tray icons are visible in the panel.
- They may flicker on the applet's Clear cycle — that's a known follow-up (ClipByChildren — see Risk register). Visibility being intermittent still counts as "the source fix landed correctly"; persistent invisibility means the fix didn't take effect.

### Task 5.3: Confirm damage volume

- [ ] **Step 1: Compare against pre-fix baseline**

```
APPLET_CONN=$(grep -E "ChangeProperty.*WM_NAME.*notification-area-applet" mate.xtrace | head -1 | cut -d: -f1)
grep -cE "^${APPLET_CONN}:>:.*Event DAMAGE-Notify" mate.xtrace
grep -cE "^${APPLET_CONN}:<:.*RENDER-Request\(133,26\)" mate.xtrace
```

Pre-fix baseline (2026-05-31, 14 s session): ~860 damage events, ~1300 FillRectangles on the applet conn.

Two acceptable outcomes:
- **Loop fully quieted.** Damage ≤30, FillRectangles ≤50 over 15 s. Done.
- **Loop persists** (still hundreds of events). The source fix is in, but the Clear-then-wipe cycle continues — that's the ClipByChildren follow-up. Tray icons should still flicker visibly during the read phase of each cycle. Stop and re-plan against the ClipByChildren scope.

### Task 5.4: Update the memory record

- [ ] **Step 1: Find the file**

```
ls /home/jos/.claude/projects/-home-jos-Projects-yserver/memory/project_tray_damage_self_loop*.md
```

- [ ] **Step 2: Add a status line**

Edit the body, prepend a "PARTIAL FIX YYYY-MM-DD in <commit>: source-picture redirect resolution landed; ClipByChildren on dst still pending if loop persists." line. If the loop fully quieted, change "PARTIAL FIX" to "FIXED".

- [ ] **Step 3: Commit**

```
git add /home/jos/.claude/projects/-home-jos-Projects-yserver/memory/project_tray_damage_self_loop.md
git commit -m "memory(tray-loop): record source-picture redirect fix status"
```

---

## Out of scope (explicit non-goals)

- **ClipByChildren on the destination paint.** When the systray applet does `FillRectangles op=Clear` on `picture(proxy)`, Xorg's `subwindow-mode = ClipByChildren` default (`xserver/render/picture.c:719`) makes the effective region empty (children fully cover the proxy) → no actual write to the backing → no damage. yserver currently writes through to the backing unconditionally. After this plan's source-side fix, that Clear still wipes the icon's pixels in the backing every cycle, which means the icons may flicker (visible during read phase, blank during Clear phase). If that materializes after Phase 5, a follow-up plan implements picture-level ClipByChildren clipping on the dst path. Out of scope here because the source-side fix is the load-bearing one for invisibility AND the loop's positive-feedback driver (empty read → composite empty → trigger reflex Clear → damage → repeat).

- **v1 (`host_x11`) backend.** The bee/MATE run uses v2. v1 already returns `supports_redirect_activation = false` and has no descendant routing. If a v1 user hits the same symptom, the same shape of fix would apply, but the work isn't in scope here.

- **Core-layer routing changes (`yserver-core::host_drawable_target`).** Core's `host_drawable_target` does not need to walk ancestors because the v2 backend does the walk via `resolve_paint_target`. Mixing core-layer offset arithmetic with backend-layer offset arithmetic would double-shift descendant paints. Don't touch core in this plan.

---

## Risk register

- **Flicker after the source fix.** As above. Probability: moderate. If it appears, document with a screen recording from bee and open the ClipByChildren follow-up plan. The applet may already work-around this if its draw cycle reads → composites → blits to the visible tray BEFORE issuing the Clear, in which case the user perception is "icons visible" even though the backing flickers internally. Live observation needed.

- **Source clip translation.** `render_composite` folds source-picture client clips into the composite clip via `compute_render_composite_clip` (`backend.rs:9932`), using `src_translation` = `dst_origin - src_origin`. The post-fix translation has the source offset added in both numerator and denominator (Task 2.1's adjustment); audit the test at `backend.rs:13661` (`poly_fill_rectangle_honours_gc_clip`) and the composite-clip tests for any assumption broken by the new arithmetic. If a test fails after Phase 2, the clip translation pivot is the first place to look.

- **Component-alpha mask path.** Component-alpha sources sometimes need separate red/green/blue scalar sampling; the engine pulls these from the same source storage. The offset applies uniformly, so component-alpha shouldn't regress — but the existing `mask_component_alpha` tests should pass post-Phase-2 without modification. If they don't, the component-alpha sample path is reading from a different `host_xid` than the resolve path; chase that.

- **Source picture wrapping a Pixmap-backed picture inherited from a redirect.** A few clients (Firefox shadow-blit) create a picture on a Pixmap, then composite that picture onto a redirected window. The source-side resolution for a Pixmap drawable correctly returns `(0, 0)` offset (Pixmaps aren't in `windows_v2`, `resolve_paint_target` returns identity via the early-return at `backend.rs:1390-1400`). No further action; just keep this in mind when reading the resolve method.
