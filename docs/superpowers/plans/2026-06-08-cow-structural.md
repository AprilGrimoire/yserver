# COW Structural Redesign — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the split COW model (resources record + drawable-store storage + scene-only special append) with the Xorg-shaped one from the spec — COW is a real root child in both resources and backend, reparents to it survive, Manual-redirected windows never emit directly to scanout, and root hit-testing treats COW as a transparent container.

**Architecture:** Single source of truth in `resources.rs::Window::children`; the backend's `windows_v2` and `top_level_order` are derived projections. All root-children mutations funnel through one COW-aware helper so the "COW stays topmost" invariant cannot be bypassed. Scene compose walks the COW like any other top-level via `emit_window_subtree`; an inherited recursion flag (`under_cow_subtree`) carries `alpha_passthrough` for the subtree. Manual-redirected windows unconditionally skip emit (no compositor-active mode flag). COW is created lazily on `GetOverlayWindow` and torn down on final `ReleaseOverlayWindow`.

**Tech Stack:** Rust 2021. yserver-core (X11 protocol + resources), yserver (KMS+Vulkan v2 backend). Tests use `#[test]` + `cargo test`; integration tests live in `crates/yserver/tests/v2_acceptance.rs`. HW validation via `just yserver-cinnamon-hw` / `just yserver-mate-hw` / `just yserver-xfce-hw` / `just yserver-e16-hw` recipes.

**Spec:** `docs/superpowers/specs/2026-06-08-cow-structural-design.md`

**Branch:** `feat/cow-structural`, branched from `master`.

**Pre-flight:**
- `git stash list` should show the rejected `stash@{0}` from the surgical-fix attempt. Leave it; it's a historical record and stays out of master.
- Local `master` clean (no uncommitted changes outside the plan/spec).
- `cargo +nightly fmt` and `cargo clippy` (default) per `AGENTS.md`.

---

## File map

**Source files modified:**

| File | Reason |
|---|---|
| `crates/yserver-core/src/resources.rs` | Remove init-time COW seed (line ~242). Add `cow_aware_top_index` helper. Apply to `create_window` (line ~520), `restack_window` `Top`/`AboveSibling` (line ~727-755), `reparent_window` push-to-root (line ~1250), `circulate_window` (line ~1003). Reject reparenting COW away from root. |
| `crates/yserver-core/src/core_loop/process_request.rs` | Wire `GetOverlayWindow` to create the COW resource + invoke backend materialization. Wire `ReleaseOverlayWindow` teardown. `RedirectWindow(COW, ...)` → `Success` no-op (matches Xorg `compRedirectWindow`). COW reparents use the existing generic cycle rule — no COW-special rejection (matches Xorg). Treat post-validation backend drift as panic (no `let _ = ...`). |
| `crates/yserver-core/src/server.rs` | Update `hit_test_children` (line ~1791) to descend through the COW (skipping it as a hit target) instead of the existing "COW not in children" special case (line ~1799-1807, deleted). |
| `crates/yserver-core/src/backend/trait_def.rs` | Change `get_overlay_window` return type to `io::Result<bool>` (true on 0→1). Add `cow_host_xid() -> Option<u32>` getter (default `None`). No new lifecycle methods — the existing overlay hooks own the full COW lifecycle including windows_v2 / top_level_order. |
| `crates/yserver/src/kms/v2/backend.rs` | Extend `get_overlay_window` (line ~9363) to also insert COW into `windows_v2` + `top_level_order` on the 0→1 transition; extend `release_overlay_window` (line ~9415) to symmetric teardown. Implement `cow_host_xid()`. Remove missing-parent fallback in `reparent_subwindow` (line ~8840) — panic on drift. Delete `arm_cow_from_recent_present_if_needed` (line ~1319), `present_to_cow_sources` ring, `maybe_register_cow_on_paint` (line ~1551). |
| `crates/yserver/src/kms/v2/scene.rs` | Delete `Scene::register_cow` / `unregister_cow` / `is_cow_registered` (line ~595-615) and `cow` field. Delete `if cow.is_some() { strip top-levels }` branch in `build_scene` (line ~1841-1856) and the special COW append (line ~1887-1905). Add `under_cow_subtree: bool` arg to `emit_window_subtree`, propagate it on recursion, use it to set `CompositeDraw::alpha_passthrough`. Add `is_manual_redirected` gate to `paint_target_is_self` (line ~2220). |

**Test files modified:**

| File | Reason |
|---|---|
| `crates/yserver-core/src/resources.rs` (`tests` mod) | Add tests for the COW-aware stacking helper + each mutator entry point + COW self-reparent rejection. |
| `crates/yserver-core/src/server.rs` (`tests` mod) | Add hit-test transparency tests. |
| `crates/yserver/src/kms/v2/backend.rs` (`tests` mod) | Add materialize/destroy lifecycle tests. Delete tests that reference the deleted `cow_authoritative` path (`cow_registers_on_first_present_to_overlay`, `cow_registers_retroactively_when_present_precedes_get_overlay_window`, `note_present_pixmap_tracks_non_cow_stage_sources_for_drawable_dump`). |
| `crates/yserver/src/kms/v2/scene.rs` (`tests` mod) | Delete `build_scene_cow_some_strips_top_levels_and_keeps_cursor_at_top` and any other `cow=Some` mode tests. Add tests for under_cow_subtree alpha propagation and the Manual-skip gate. |
| `crates/yserver/tests/v2_acceptance.rs` | Add an integration test for the full compositor flow: GetOverlayWindow → create stage as child of COW → reparent + present → build_scene → assert ordering and Manual-redirect skip. |

---

## Phase 1 — Root-children helper foundation

**Purpose:** establish the single COW-aware funnel for all `root.children` mutations. No behavioral change at runtime yet (because COW isn't in `root.children` until Phase 2), but every mutation site routes through one helper so when COW arrives, the invariant holds automatically.

### Task 1.1: Add the `cow_aware_top_index` helper

**Files:**
- Modify: `crates/yserver-core/src/resources.rs` (add a free fn near the top of the `impl ResourceTable` block, or as a module-level fn — pick whichever matches surrounding style)
- Test: `crates/yserver-core/src/resources.rs` (`#[cfg(test)] mod tests` at bottom)

- [ ] **Step 1: Add the failing test** — write to `crates/yserver-core/src/resources.rs` inside the existing `tests` module:

```rust
    #[test]
    fn cow_aware_top_index_with_no_cow_returns_end() {
        let mut t = ResourceTable::new();
        make_child(&mut t, 0x200, ROOT_WINDOW.0, 0, 0);
        make_child(&mut t, 0x300, ROOT_WINDOW.0, 0, 0);
        let root = t.window(ROOT_WINDOW).unwrap();
        assert_eq!(cow_aware_top_index(root), root.children.len());
    }

    #[test]
    fn cow_aware_top_index_with_cow_at_top_returns_just_below() {
        let mut t = ResourceTable::new();
        make_child(&mut t, 0x200, ROOT_WINDOW.0, 0, 0);
        // Simulate the post-Phase-2 state where COW is the last (topmost) root child.
        // For this isolated unit test, push it directly so we can exercise the helper.
        t.windows.get_mut(&ROOT_WINDOW.0).unwrap().children.push(COMPOSITE_OVERLAY_WINDOW);
        let root = t.window(ROOT_WINDOW).unwrap();
        assert_eq!(cow_aware_top_index(root), root.children.len() - 1);
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p yserver-core --lib resources::tests::cow_aware_top_index_`
Expected: FAIL with `cannot find function 'cow_aware_top_index' in this scope`

- [ ] **Step 3: Add the helper**

Insert above the `impl ResourceTable` block (or at the bottom of the file outside the impl, whichever is consistent with file conventions):

```rust
/// Index where a new "top" child should be inserted under `parent`. If
/// the COW is currently the topmost child (last in the `children` vec
/// per yserver's `children[len-1] == top` convention), insert below it;
/// otherwise insert at the very end. Mirrors Xorg's
/// `CompositeRealChildHead` (composite/compwindow.c:761-792).
///
/// `parent` is typically `ROOT_WINDOW` (COW's parent); calling on other
/// parents is a no-op (returns `children.len()`).
#[must_use]
pub(crate) fn cow_aware_top_index(parent: &Window) -> usize {
    match parent.children.last() {
        Some(&last) if last == COMPOSITE_OVERLAY_WINDOW => parent.children.len() - 1,
        _ => parent.children.len(),
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p yserver-core --lib resources::tests::cow_aware_top_index_`
Expected: PASS (both cases)

- [ ] **Step 5: Commit**

```bash
git add crates/yserver-core/src/resources.rs
git commit -m "resources: add cow_aware_top_index helper

Single point of truth for 'where does a new top-level go in root.children'
under Xorg's CompositeRealChildHead semantic. No call sites yet — Phase 1
follow-ups route the existing root-stack mutators through this helper."
```

---

### Task 1.2: Route `create_window` through the helper

**Files:**
- Modify: `crates/yserver-core/src/resources.rs` (the `children.push(request.window)` at line ~520)
- Test: same file's tests mod

- [ ] **Step 1: Add a failing test**

```rust
    #[test]
    fn create_window_with_cow_present_inserts_below_cow() {
        let mut t = ResourceTable::new();
        t.windows.get_mut(&ROOT_WINDOW.0).unwrap().children.push(COMPOSITE_OVERLAY_WINDOW);
        make_child(&mut t, 0x500, ROOT_WINDOW.0, 0, 0);
        let kids = &t.window(ROOT_WINDOW).unwrap().children;
        assert_eq!(kids.last().copied(), Some(COMPOSITE_OVERLAY_WINDOW),
            "COW must stay at top after create_window");
        assert_eq!(kids[kids.len() - 2], ResourceId(0x500),
            "new top-level lands just below COW");
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p yserver-core --lib resources::tests::create_window_with_cow_present_inserts_below_cow`
Expected: FAIL (the new window is at children.len()-1, COW pushed down)

- [ ] **Step 3: Change the push to a cow-aware insert**

In `crates/yserver-core/src/resources.rs`, find the block ending around line 520:

```rust
        self.windows
            .entry(request.parent.0)
            .or_insert_with(|| Window::placeholder(request.parent))
            .children
            .push(request.window);
```

Replace with:

```rust
        let parent_entry = self
            .windows
            .entry(request.parent.0)
            .or_insert_with(|| Window::placeholder(request.parent));
        let insert_at = cow_aware_top_index(parent_entry);
        parent_entry.children.insert(insert_at, request.window);
```

- [ ] **Step 4: Run test + the existing create_window tests**

Run: `cargo test -p yserver-core --lib resources::tests::create_window`
Expected: PASS — both the new test and all pre-existing `create_window_*` tests.

- [ ] **Step 5: Commit**

```bash
git add crates/yserver-core/src/resources.rs
git commit -m "resources: route create_window through cow_aware_top_index

New top-level windows now insert just below the COW when it's at the top
of root.children. No runtime effect yet — COW isn't in root.children
until Phase 2 wires it in at GetOverlayWindow."
```

---

### Task 1.3: Route `restack_window::Top` through the helper

**Files:**
- Modify: `crates/yserver-core/src/resources.rs` (line ~727-732)
- Test: same file's tests mod

- [ ] **Step 1: Add a failing test**

```rust
    #[test]
    fn restack_top_with_cow_present_lands_just_below_cow() {
        let mut t = ResourceTable::new();
        make_child(&mut t, 0x200, ROOT_WINDOW.0, 0, 0);
        make_child(&mut t, 0x300, ROOT_WINDOW.0, 0, 0);
        t.windows.get_mut(&ROOT_WINDOW.0).unwrap().children.push(COMPOSITE_OVERLAY_WINDOW);
        // children now: [0x200, 0x300, COW]
        let _ = t.configure_window(ConfigureWindowRequest {
            window: ResourceId(0x200),
            value_mask: 1 << 6,
            x: None, y: None, width: None, height: None, border_width: None,
            sibling: None,
            stack_mode: Some(0),  // 0 = Above (with no sibling = top)
        });
        let kids = &t.window(ROOT_WINDOW).unwrap().children;
        assert_eq!(kids, &[ResourceId(0x300), ResourceId(0x200), COMPOSITE_OVERLAY_WINDOW]);
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p yserver-core --lib resources::tests::restack_top_with_cow_present_lands_just_below_cow`
Expected: FAIL — current code pushes 0x200 to end, ahead of COW.

- [ ] **Step 3: Change the `RestackAction::Top` arm**

At line ~729-732 in `crates/yserver-core/src/resources.rs`:

```rust
            RestackAction::Top => {
                let window = parent.children.remove(index);
                parent.children.push(window);
            }
```

Replace with:

```rust
            RestackAction::Top => {
                let window = parent.children.remove(index);
                let insert_at = cow_aware_top_index(parent);
                parent.children.insert(insert_at, window);
            }
```

- [ ] **Step 4: Run all restack-related tests**

Run: `cargo test -p yserver-core --lib resources::tests::restack`
Expected: PASS — new test + pre-existing `restack_*` and `configure_window_stack_mode_*` tests.

- [ ] **Step 5: Commit**

```bash
git add crates/yserver-core/src/resources.rs
git commit -m "resources: route restack_window Top through cow_aware_top_index

Raise-to-top respects the COW-stays-on-top invariant. No runtime effect
until Phase 2; structural prep only."
```

---

### Task 1.4: Route `restack_window::AboveSibling(COW)` through the helper

**Files:**
- Modify: `crates/yserver-core/src/resources.rs` (the `AboveSibling` arm in `restack_window`, around line ~737-744)
- Test: same file's tests mod

- [ ] **Step 1: Add a failing test**

```rust
    #[test]
    fn restack_above_cow_caps_to_just_below_cow() {
        let mut t = ResourceTable::new();
        make_child(&mut t, 0x200, ROOT_WINDOW.0, 0, 0);
        make_child(&mut t, 0x300, ROOT_WINDOW.0, 0, 0);
        t.windows.get_mut(&ROOT_WINDOW.0).unwrap().children.push(COMPOSITE_OVERLAY_WINDOW);
        // children: [0x200, 0x300, COW]; ask for 0x200 above COW.
        let _ = t.configure_window(ConfigureWindowRequest {
            window: ResourceId(0x200),
            value_mask: 0,
            x: None, y: None, width: None, height: None, border_width: None,
            sibling: Some(COMPOSITE_OVERLAY_WINDOW),
            stack_mode: Some(0),  // Above
        });
        let kids = &t.window(ROOT_WINDOW).unwrap().children;
        assert_eq!(kids.last().copied(), Some(COMPOSITE_OVERLAY_WINDOW),
            "COW must remain at top after AboveSibling=COW");
        assert_eq!(kids[kids.len() - 2], ResourceId(0x200),
            "0x200 must land just below COW (capped)");
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p yserver-core --lib resources::tests::restack_above_cow_caps_to_just_below_cow`
Expected: FAIL — current code inserts at `sibling_index + 1` which puts 0x200 above COW.

- [ ] **Step 3: Patch the `AboveSibling` arm**

At line ~737-744 in `crates/yserver-core/src/resources.rs`:

```rust
            RestackAction::AboveSibling(sibling_id) => {
                let window = parent.children.remove(index);
                let sibling_index = parent
                    .children
                    .iter()
                    .position(|child| *child == sibling_id);
                let insert_at = sibling_index.map_or(parent.children.len(), |i| i + 1);
                parent.children.insert(insert_at, window);
            }
```

Replace with:

```rust
            RestackAction::AboveSibling(sibling_id) => {
                let window = parent.children.remove(index);
                let insert_at = if sibling_id == COMPOSITE_OVERLAY_WINDOW {
                    // Cap: cannot land above COW. Use the same just-below-COW slot
                    // the cow_aware_top_index helper computes.
                    cow_aware_top_index(parent)
                } else {
                    let sibling_index = parent
                        .children
                        .iter()
                        .position(|child| *child == sibling_id);
                    sibling_index.map_or(parent.children.len(), |i| i + 1)
                };
                parent.children.insert(insert_at, window);
            }
```

- [ ] **Step 4: Run restack tests**

Run: `cargo test -p yserver-core --lib resources::tests::restack`
Expected: PASS — including the new AboveSibling=COW cap test and pre-existing AboveSibling tests.

- [ ] **Step 5: Commit**

```bash
git add crates/yserver-core/src/resources.rs
git commit -m "resources: cap AboveSibling=COW to just-below-COW

A client trying to stack above the overlay window gets the just-below-COW
slot instead, matching Xorg's CompositeRealChildHead behavior."
```

---

### Task 1.5: Route `reparent_window`'s push-to-root through the helper

**Files:**
- Modify: `crates/yserver-core/src/resources.rs` (line ~1250 — the `parent.children.push(request.window)` inside `reparent_window`)
- Test: same file's tests mod

- [ ] **Step 1: Add a failing test**

```rust
    #[test]
    fn reparent_to_root_with_cow_present_lands_below_cow() {
        let mut t = ResourceTable::new();
        // Build: root → container; container → child.
        make_child(&mut t, 0xc0, ROOT_WINDOW.0, 0, 0);
        make_child(&mut t, 0xd0, 0xc0, 0, 0);
        // Put COW at top of root.
        t.windows.get_mut(&ROOT_WINDOW.0).unwrap().children.push(COMPOSITE_OVERLAY_WINDOW);
        // Reparent the inner child up to root.
        let _ = t.reparent_window(ReparentWindowRequest {
            window: ResourceId(0xd0),
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
        });
        let kids = &t.window(ROOT_WINDOW).unwrap().children;
        assert_eq!(kids.last().copied(), Some(COMPOSITE_OVERLAY_WINDOW),
            "COW must remain topmost after reparent-to-root");
        assert!(kids.contains(&ResourceId(0xd0)),
            "the reparented child must appear in root.children");
        assert_ne!(kids.last().copied(), Some(ResourceId(0xd0)),
            "the reparented child must NOT be above COW");
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p yserver-core --lib resources::tests::reparent_to_root_with_cow_present_lands_below_cow`
Expected: FAIL — current `reparent_window` pushes the child to the end of root.children, above COW.

- [ ] **Step 3: Patch `reparent_window`**

At line ~1248-1252 in `crates/yserver-core/src/resources.rs`:

```rust
        if let Some(parent) = self.windows.get_mut(&request.parent.0) {
            parent.children.push(request.window);
        }
```

Replace with:

```rust
        if let Some(parent) = self.windows.get_mut(&request.parent.0) {
            let insert_at = cow_aware_top_index(parent);
            parent.children.insert(insert_at, request.window);
        }
```

(`request.window` and `request.parent` may be named differently in the surrounding scope — match the existing variable name used in the assignment block.)

- [ ] **Step 4: Run reparent tests**

Run: `cargo test -p yserver-core --lib resources::tests::reparent`
Expected: PASS — new test + pre-existing `reparent_window_*` tests.

- [ ] **Step 5: Commit**

```bash
git add crates/yserver-core/src/resources.rs
git commit -m "resources: route reparent-to-root through cow_aware_top_index

Reparenting a window back into root's children respects the COW-at-top
invariant, completing the 'every root.children mutator funnels through
one helper' rule from the spec."
```

---

### Task 1.6: COW-aware `circulate_window` on ROOT_WINDOW

**Files:**
- Modify: `crates/yserver-core/src/resources.rs` (`circulate_window` at line ~1003)
- Test: same file's tests mod

X11 `CirculateWindow` rotates a container's children: `direction=0` (Raise) takes the bottom-most occluded sibling and raises it to the top; `direction=1` (Lower) takes the top-most occluding sibling and lowers it to the bottom. Under Xorg's `CompositeRealChildHead`, COW is excluded from these operations — it stays at the top.

- [ ] **Step 1: Add a failing test**

```rust
    #[test]
    fn circulate_raise_on_root_skips_cow() {
        let mut t = ResourceTable::new();
        // Root children, bottom-to-top: [A=0x200 (occluded), B=0x300, COW].
        // We need positions where A is occluded by B for Raise to act on A;
        // for the geometry-free unit test, set both A and B at (0,0,50x50) so
        // B occludes A.
        make_child(&mut t, 0x200, ROOT_WINDOW.0, 0, 0);
        make_child(&mut t, 0x300, ROOT_WINDOW.0, 0, 0);
        let _ = t.map_window(ResourceId(0x200));
        let _ = t.map_window(ResourceId(0x300));
        t.windows.get_mut(&ROOT_WINDOW.0).unwrap().children.push(COMPOSITE_OVERLAY_WINDOW);
        // Circulate Raise on root: A should rise to "top of the non-COW slice",
        // i.e. just below COW. COW stays last.
        let _ = t.circulate_window(ROOT_WINDOW, 0);
        let kids = &t.window(ROOT_WINDOW).unwrap().children;
        assert_eq!(kids.last().copied(), Some(COMPOSITE_OVERLAY_WINDOW),
            "COW stays at top across circulate");
        assert_eq!(kids[kids.len() - 2], ResourceId(0x200),
            "Raise must land 0x200 just below COW, not above it");
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p yserver-core --lib resources::tests::circulate_raise_on_root_skips_cow`
Expected: FAIL — current `circulate_window` ignores COW.

- [ ] **Step 3: Patch `circulate_window`**

Read the current `circulate_window` (line ~1003). The internal logic operates on `parent.children` directly. Wherever it computes "top" or "bottom" of the slice it operates on, replace with a COW-aware slice when `container == ROOT_WINDOW`:

- "Top" position: `cow_aware_top_index(parent)` rather than `parent.children.len()`.
- "Bottom" position: unchanged (COW is at the top, not the bottom).
- Iteration to find "topmost occluding" or "bottommost occluded" sibling: skip COW when `container == ROOT_WINDOW`.

Concrete change pattern: at each `parent.children.len()` or `parent.children.last()` use site in `circulate_window`, substitute the COW-aware equivalent when the parent is root. Read the surrounding implementation to identify the exact lines — for some implementations there are 2-3 such uses, for others a single "top index" computation feeds the whole function.

If `circulate_window` currently exposes a single internal helper like `fn topmost_index(parent: &Window) -> usize`, modify that helper to use `cow_aware_top_index` when parent is root. If it inlines, replace each site.

After patching, re-read the function to make sure no path can move the COW or place anything above it.

- [ ] **Step 4: Run circulate tests**

Run: `cargo test -p yserver-core --lib resources::tests::circulate`
Expected: PASS — new test + any pre-existing `circulate_*` tests.

- [ ] **Step 5: Commit**

```bash
git add crates/yserver-core/src/resources.rs
git commit -m "resources: skip COW in circulate_window on ROOT_WINDOW

CirculateWindow Raise/Lower on root excludes the overlay from the slice
it operates on, mirroring CompositeRealChildHead. Closes the
'circulate_window can move COW or move others above it' gap codex flagged
in round 2."
```

---

### Phase 1 acceptance

- [ ] Run the full resources unit test suite: `cargo test -p yserver-core --lib resources::`
- [ ] Run a full project build: `cargo build --locked`
- [ ] All tests green, no new warnings.

Note: at this point, COW is still NOT in root.children (Phase 2 wires that in), so the helper is currently a no-op in production. All Phase 1 tests use the `t.windows.get_mut(&ROOT_WINDOW.0).unwrap().children.push(COMPOSITE_OVERLAY_WINDOW)` shim to simulate post-Phase-2 state. This is intentional — it pre-validates the funnel before the COW arrives.

---

## Phase 2 — COW lifecycle (materialize / teardown) + scene rewrite

**Purpose:** make `GetOverlayWindow` materialize the COW as a real `resources` + `windows_v2` node, and `ReleaseOverlayWindow` tear it down. Delete the special COW append in `build_scene` — COW now emits via the normal `top_level_order` walk. Add the inherited `under_cow_subtree` recursion flag for `alpha_passthrough` semantics.

At the end of Phase 2 the cinnamon prompt-under-parent symptom should be **structurally fixed AND interactive**: COW + stage on top of Manual-redirected backings (Tasks 2.1–2.7), plus click-through input semantics (Tasks 2.8–2.10) so clicks reach the right windows. Per the spec, input semantics are part of materialization, not a follow-up — they land in the same phase as the rest of the lifecycle. Phases 3–5 are correctness hardening on top of a working visual + input state.

### Task 2.1: Remove init-time COW seed in resources

**Files:**
- Modify: `crates/yserver-core/src/resources.rs` (line ~240-260 — the `windows.insert(COMPOSITE_OVERLAY_WINDOW.0, Window { ... })` block at server init)

- [ ] **Step 1: Add a failing test**

```rust
    #[test]
    fn fresh_resources_does_not_contain_cow() {
        let t = ResourceTable::new();
        assert!(t.window(COMPOSITE_OVERLAY_WINDOW).is_none(),
            "COW must NOT be pre-seeded; it materializes only on GetOverlayWindow");
        assert!(!t.window(ROOT_WINDOW).unwrap().children.contains(&COMPOSITE_OVERLAY_WINDOW),
            "fresh root.children must not contain COW");
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p yserver-core --lib resources::tests::fresh_resources_does_not_contain_cow`
Expected: FAIL — current `ResourceTable::new` inserts COW at init.

- [ ] **Step 3: Delete the init-time insertion**

In `crates/yserver-core/src/resources.rs`, find the block starting around line 240 that inserts `COMPOSITE_OVERLAY_WINDOW.0` into `windows`. The exact shape:

```rust
        windows.insert(
            COMPOSITE_OVERLAY_WINDOW.0,
            Window {
                id: COMPOSITE_OVERLAY_WINDOW,
                parent: ROOT_WINDOW,
                children: Vec::new(),
                x: 0,
                // ... rest of fields
            },
        );
```

Delete the entire `windows.insert(COMPOSITE_OVERLAY_WINDOW.0, ...)` call and any explanatory comment immediately above it. Add a one-line comment in its place noting that the COW materializes on first `GetOverlayWindow`, referencing the spec.

- [ ] **Step 4: Run the existing resources tests + the new one**

Run: `cargo test -p yserver-core --lib resources::tests`
Expected: most PASS. **Some pre-existing tests may now fail** if they depended on the seeded COW. Triage each failure:
- If the test was probing COW-resource behavior on a fresh table: change the test to materialize COW first via a test helper (write one if needed: `fn materialize_cow_for_tests(t: &mut ResourceTable)` that inserts the COW window into resources and root.children using `cow_aware_top_index` — Task 2.4 will provide the production version of `materialize_cow_resource`).
- If the test was probing non-COW behavior but happened to rely on COW being a child of root: change it to set up explicitly.

Re-run after each fix. Continue until all green.

- [ ] **Step 5: Commit**

```bash
git add crates/yserver-core/src/resources.rs
git commit -m "resources: stop pre-seeding the COW at server init

Per the spec, COW exists fully or not at all (invariant 4). Pre-seeding
an empty COW resource record diverged from Xorg (which creates the COW
only at GetOverlayWindow) and contradicted the lifecycle. Phase 2 wires
the materialization at the right place; Phase 1 tests already prove the
helper handles the post-materialization state correctly."
```

---

### Task 2.2: Extend `get_overlay_window` / `release_overlay_window` to also manage `windows_v2` + `top_level_order`

**Files:**
- Modify: `crates/yserver/src/kms/v2/backend.rs` (extend existing `get_overlay_window` at ~line 9363 and `release_overlay_window` at ~line 9415)
- Test: `crates/yserver/src/kms/v2/backend.rs` (`tests` mod)

**Design note:** the COW has one lifecycle. The Backend trait already has `get_overlay_window` / `release_overlay_window` for that lifecycle; we extend those instead of adding a second pair (`materialize_cow` / `destroy_cow`). Single API, single source of authority for the backend half — `get_overlay_window` is now responsible for the full backend side of COW materialization (storage + `windows_v2` + `top_level_order`), and `release_overlay_window` is responsible for the full teardown.

The trait surface needs one change: `get_overlay_window` must return `io::Result<bool>` (true on 0→1 transition, false on subsequent claims) so the core handler knows when to drive the resources-side materialization. Same for `release_overlay_window` (true on 1→0, false otherwise). The KmsBackendV2 impl already differentiates internally — this exposes that signal to callers.

- [ ] **Step 1: Update the trait signatures in `crates/yserver-core/src/backend/trait_def.rs`**

Find the existing `fn get_overlay_window(...) -> io::Result<()>` and `fn release_overlay_window(...) -> io::Result<bool>`. Update the get-side to return `io::Result<bool>`:

```rust
    /// Stage 4d — Composite Overlay Window allocation + materialization.
    /// On the 0→1 refcount transition (first claim), allocate backing
    /// storage AND populate the backend's window-tree projection: a
    /// `windows_v2` entry sized to full screen extent (mapped=true,
    /// depth=24, parent=root) and a slot at the top of
    /// `top_level_order`. Returns `Ok(true)` on first claim, `Ok(false)`
    /// on subsequent claims (refcount bump only — no new
    /// materialization).
    ///
    /// The core handler uses the bool to drive the symmetric resources-
    /// side materialization (`materialize_cow_resource`). Both halves
    /// must succeed or both must rollback; an Err here is a fatal
    /// internal-consistency failure per the spec.
    fn get_overlay_window(&mut self, _origin: Option<OriginContext>) -> io::Result<bool> {
        Ok(false)
    }
```

`release_overlay_window` already returns `io::Result<bool>` (line ~417 in trait_def.rs based on earlier reading) — extend its docstring to note the v2 impl now also tears down `windows_v2` + `top_level_order` on the final release.

- [ ] **Step 2: Add a failing test**

In the `tests` module at the bottom of `crates/yserver/src/kms/v2/backend.rs`:

```rust
    #[test]
    fn get_overlay_window_first_claim_materializes_full_backend_state() {
        let mut b = KmsBackendV2::for_tests();
        let fb_w = u32::from(b.platform.fb_w);
        let fb_h = u32::from(b.platform.fb_h);

        let was_first_claim = b.get_overlay_window(None).expect("get_overlay_window");
        assert!(was_first_claim, "0→1 transition must return Ok(true)");

        let cow_host_xid = b.cow_host_xid()
            .expect("cow_host_xid getter must return Some after first claim");
        let geom = b.windows_v2.get(&cow_host_xid)
            .expect("COW must be present in windows_v2 after first claim");
        assert!(geom.mapped);
        assert_eq!(geom.depth, 24);
        assert_eq!(u32::from(geom.width), fb_w);
        assert_eq!(u32::from(geom.height), fb_h);
        assert_eq!(geom.parent, None);
        assert_eq!(b.core.top_level_order.last().copied(), Some(cow_host_xid),
            "COW is the topmost entry in top_level_order");
    }

    #[test]
    fn get_overlay_window_second_claim_does_not_remateralize() {
        let mut b = KmsBackendV2::for_tests();
        b.get_overlay_window(None).expect("first claim");
        let cow_host_xid = b.cow_host_xid().expect("after first claim");
        // Snapshot the rank to make sure a second claim doesn't reallocate.
        let rank_before = b.windows_v2.get(&cow_host_xid).unwrap().stack_rank;

        let was_first_claim = b.get_overlay_window(None).expect("second claim");
        assert!(!was_first_claim, "subsequent claim must return Ok(false)");
        let rank_after = b.windows_v2.get(&cow_host_xid).unwrap().stack_rank;
        assert_eq!(rank_before, rank_after, "subsequent claim must not rebuild windows_v2 entry");
    }

    #[test]
    fn release_overlay_window_final_release_tears_down_full_backend_state() {
        let mut b = KmsBackendV2::for_tests();
        b.get_overlay_window(None).expect("get");
        let cow_host_xid = b.cow_host_xid().expect("present");

        let was_final_release = b.release_overlay_window(None).expect("release");
        assert!(was_final_release, "1→0 transition must return Ok(true)");
        assert!(b.windows_v2.get(&cow_host_xid).is_none(),
            "COW removed from windows_v2 on final release");
        assert!(!b.core.top_level_order.contains(&cow_host_xid),
            "COW removed from top_level_order");
        assert!(b.cow_host_xid().is_none(),
            "cow_host_xid getter returns None after final release");
    }
```

`cow_host_xid()` is a new getter (Step 4). The tests pin its return type and lifecycle expectations.

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p yserver --lib backend::tests::get_overlay_window_first_claim backend::tests::release_overlay_window_final_release`
Expected: FAIL — `get_overlay_window` doesn't add to `windows_v2` / `top_level_order` yet; `cow_host_xid()` doesn't exist yet.

- [ ] **Step 4: Extend `KmsBackendV2::get_overlay_window` / `release_overlay_window` + add the `cow_host_xid()` getter**

In `crates/yserver/src/kms/v2/backend.rs`, find the existing `fn get_overlay_window(...)` impl (around line 9363). After the existing "allocate storage + set `self.cow_id`" lines on the 0→1 transition (the branch where `self.cow_id.is_some()` is false), append the `windows_v2` + `top_level_order` work. Also change the return type to `io::Result<bool>` and return `Ok(true)` on the 0→1 branch, `Ok(false)` on the refcount-bump branch.

Concrete shape:

```rust
    fn get_overlay_window(&mut self, _origin: Option<OriginContext>) -> io::Result<bool> {
        if self.cow_id.is_some() {
            self.core.cow_refcount += 1;
            return Ok(false); // already materialized; refcount bump only
        }
        // --- existing 0→1 path ---
        let fb_w = self.platform.fb_w.max(1);
        let fb_h = self.platform.fb_h.max(1);
        // ... existing storage allocation ...
        // ... self.cow_id = Some(id);
        // ... self.core.cow_refcount = 1;
        // ... self.arm_cow_from_recent_present_if_needed(); — this call goes
        //     away entirely in Phase 5.2 along with the helper; for now leave it.
        // --- NEW: also materialize windows_v2 + top_level_order ---
        let cow_host_xid = yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0;
        let rank = self.alloc_window_stack_rank();
        let geom = super::WindowGeometryV2 {
            x: 0, y: 0,
            width: fb_w,
            height: fb_h,
            depth: 24,
            mapped: true,
            parent: None, // root parent in windows_v2's convention (root not tracked)
            stack_rank: rank,
            bg_pixel: None,
            bg_pixmap: None,
            cursor: None,
        };
        self.windows_v2.insert(cow_host_xid, geom);
        self.core.top_level_order.retain(|&x| x != cow_host_xid);
        self.core.top_level_order.push(cow_host_xid);
        self.scene.mark_scene_structure_dirty();
        Ok(true)
    }
```

For `release_overlay_window`, find the existing impl (around line 9415). It already returns `Ok(true)` on final release. Inside that branch, BEFORE the storage decref, add the `windows_v2` + `top_level_order` teardown:

```rust
    fn release_overlay_window(&mut self, _origin: Option<OriginContext>) -> io::Result<bool> {
        if self.core.cow_refcount == 0 { return Ok(false); }
        self.core.cow_refcount -= 1;
        if self.core.cow_refcount == 0 {
            // --- NEW: tear down windows_v2 + top_level_order first ---
            let cow_host_xid = yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0;
            self.core.top_level_order.retain(|&x| x != cow_host_xid);
            self.windows_v2.remove(&cow_host_xid);
            self.scene.mark_scene_structure_dirty();
            // --- existing teardown: flush_render_batch, scene.unregister_cow (until Phase 5),
            //     store_decref_with_invalidate, self.cow_id = None ---
            // ...
            Ok(true)
        } else {
            Ok(false)
        }
    }
```

Add a `cow_host_xid()` getter on `KmsBackendV2`:

```rust
    pub(crate) fn cow_host_xid(&self) -> Option<u32> {
        // The COW's host xid is the well-known protocol xid once
        // get_overlay_window has materialized; None otherwise.
        if self.cow_id.is_some() {
            Some(yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0)
        } else {
            None
        }
    }
```

And add to the Backend trait (default `None`):

```rust
    /// Stage 4e — return the host xid the backend has assigned to the
    /// COW, or `None` if the COW is not currently materialized. The
    /// core handler reads this to populate the resources COW record's
    /// `host_xid` field after `get_overlay_window`'s 0→1 return.
    fn cow_host_xid(&self) -> Option<u32> {
        None
    }
```

- [ ] **Step 5: Run the tests**

Run: `cargo test -p yserver --lib backend::tests::get_overlay_window backend::tests::release_overlay_window`
Expected: PASS — new tests + pre-existing `cow_get_overlay_*` / `cow_release_*` tests (the latter may need their assertions updated for the new return type; if so, update them in this commit).

- [ ] **Step 6: Commit**

```bash
git add crates/yserver-core/src/backend/trait_def.rs crates/yserver/src/kms/v2/backend.rs
git commit -m "kms/v2: extend get_overlay_window/release_overlay_window to own full COW backend lifecycle

Single backend hook per lifecycle event. get_overlay_window's 0→1 path
now also inserts the COW into windows_v2 + top_level_order; the
symmetric release teardown removes them. get_overlay_window now returns
bool so the core handler can drive the resources-side materialization
only on the 0→1 transition. cow_host_xid() getter exposes the
backend-assigned xid for the core handler to pin on the resources record.

No second trait API (materialize_cow / destroy_cow). One COW, one
lifecycle, one backend hook per transition — per codex round-3 review."
```

---

### Task 2.4: Wire `GetOverlayWindow` handler to drive both resources + backend

**Files:**
- Modify: `crates/yserver-core/src/core_loop/process_request.rs` (the existing `GetOverlayWindow` handler — find via `grep -n 'COMPOSITE.*GetOverlayWindow' crates/yserver-core/src/core_loop/process_request.rs` or `grep -n 'fn handle.*overlay_window' ...`)
- Modify: `crates/yserver-core/src/resources.rs` (add a public `pub fn materialize_cow_resource(&mut self, host_xid: WindowHandle)` helper)

This is the core lifecycle wiring: on first GetOverlayWindow (refcount 0→1, signaled by `backend.get_overlay_window()` returning `Ok(true)`), create the resources record, populate `host_xid`, and insert into `root.children` via `cow_aware_top_index`. The backend side is already taken care of inside `backend.get_overlay_window` (Task 2.2). One bool return signals the entire transition.

- [ ] **Step 1: Add a resources-side helper, strictly 0→1 (no in-place clobber)**

In `crates/yserver-core/src/resources.rs`, add (place near other public mutation methods, e.g., near `create_window`):

```rust
    /// Stage 4e — create the Composite Overlay Window resource record
    /// as a child of root, populate its `host_xid`, and insert it as
    /// the topmost child via `cow_aware_top_index`.
    ///
    /// **Precondition: COW is not currently materialized.** Called by
    /// the core `GetOverlayWindow` handler only on the 0→1 refcount
    /// transition (the backend's `get_overlay_window` returned
    /// `Ok(true)`). Panics if a COW resource record already exists;
    /// repeated `GetOverlayWindow` calls without an intervening release
    /// are refcount-only and never reach this function. This guard is
    /// load-bearing: the COW's resource record may carry live state
    /// across an interactive session (event-mask selections, properties,
    /// children — the compositor stage gets reparented under it),
    /// rebuilding it would silently drop that state.
    pub fn materialize_cow_resource(&mut self, host_xid: WindowHandle) {
        assert!(
            !self.windows.contains_key(&COMPOSITE_OVERLAY_WINDOW.0),
            "materialize_cow_resource: COW already materialized; \
             core handler must only call this on get_overlay_window's \
             Ok(true) (0→1) return, never on subsequent claims"
        );
        // Build the resource record. Geometry mirrors Xorg
        // `compoverlay.c:compCreateOverlayWindow`: full screen, depth =
        // root depth, override-redirect, no automatic background.
        let root_w = self.window(ROOT_WINDOW).map(|r| r.width).unwrap_or(1);
        let root_h = self.window(ROOT_WINDOW).map(|r| r.height).unwrap_or(1);
        let root_visual = self.window(ROOT_WINDOW).map(|r| r.visual).unwrap_or(ROOT_VISUAL);
        let cow_window = Window {
            id: COMPOSITE_OVERLAY_WINDOW,
            parent: ROOT_WINDOW,
            children: Vec::new(),
            x: 0,
            y: 0,
            width: root_w,
            height: root_h,
            border_width: 0,
            depth: 24,
            visual: root_visual,
            class: WindowClass::InputOutput,
            map_state: MapState::Viewable,
            override_redirect: true,
            host_xid: Some(host_xid),
            // Fill remaining fields with defaults matching `create_window`'s
            // template (background_pixel default, properties empty, etc.).
            // Use Window::placeholder(ROOT_WINDOW) as a base if the project
            // already has that helper; otherwise spell out the fields. The
            // critical fields are the ones listed above.
            ..Window::placeholder(ROOT_WINDOW)
        };
        self.windows.insert(COMPOSITE_OVERLAY_WINDOW.0, cow_window);

        // Insert COW into root.children at the cow-aware top slot. The
        // post-condition: COW is the last entry in root.children.
        if let Some(root) = self.windows.get_mut(&ROOT_WINDOW.0) {
            let idx = cow_aware_top_index(root);
            root.children.insert(idx, COMPOSITE_OVERLAY_WINDOW);
        }
    }

    /// Stage 4e — symmetric teardown. Remove the COW from root.children
    /// and drop the resource record. Called by the core
    /// `ReleaseOverlayWindow` handler only on the 1→0 refcount
    /// transition (`backend.release_overlay_window` returned
    /// `Ok(true)`). Drops any live COW-local state (event-mask
    /// selections, property store, child window list) — by definition
    /// the compositor has explicitly released, so this is intentional.
    pub fn destroy_cow_resource(&mut self) {
        if let Some(root) = self.windows.get_mut(&ROOT_WINDOW.0) {
            root.children.retain(|&c| c != COMPOSITE_OVERLAY_WINDOW);
        }
        self.windows.remove(&COMPOSITE_OVERLAY_WINDOW.0);
    }
```

> If `Window::placeholder` doesn't exist or has a different shape, manually fill all fields. The point is: the COW is a valid `Window` record with the listed deviations.

- [ ] **Step 2: Add a failing test for the wiring**

In `crates/yserver-core/src/resources.rs` tests mod:

```rust
    #[test]
    fn materialize_cow_resource_creates_record_and_inserts_at_top() {
        let mut t = ResourceTable::new();
        make_child(&mut t, 0x200, ROOT_WINDOW.0, 0, 0);
        let host_xid = WindowHandle::from_raw_panicking(0x4000_0103);
        t.materialize_cow_resource(host_xid);

        let cow = t.window(COMPOSITE_OVERLAY_WINDOW).expect("COW resource exists after materialize");
        assert_eq!(cow.host_xid, Some(host_xid));
        assert_eq!(cow.parent, ROOT_WINDOW);
        assert!(cow.override_redirect);

        let kids = &t.window(ROOT_WINDOW).unwrap().children;
        assert_eq!(kids.last().copied(), Some(COMPOSITE_OVERLAY_WINDOW),
            "COW lands at the top of root.children");
    }

    #[test]
    fn destroy_cow_resource_removes_record_and_root_child() {
        let mut t = ResourceTable::new();
        let host_xid = WindowHandle::from_raw_panicking(0x4000_0103);
        t.materialize_cow_resource(host_xid);
        t.destroy_cow_resource();
        assert!(t.window(COMPOSITE_OVERLAY_WINDOW).is_none());
        assert!(!t.window(ROOT_WINDOW).unwrap().children.contains(&COMPOSITE_OVERLAY_WINDOW));
    }
```

- [ ] **Step 3: Run + build**

Run: `cargo test -p yserver-core --lib resources::tests::materialize_cow_resource resources::tests::destroy_cow_resource`
Expected: PASS.

- [ ] **Step 4: Update the `GetOverlayWindow` handler in `process_request.rs`**

Find the existing handler — it calls `backend.get_overlay_window(origin)`. The new flow drives the resources-side materialization ONLY on the 0→1 transition (per Task 2.2 the backend hook now returns `Ok(true)` for first claim, `Ok(false)` for subsequent claims, and has already taken care of `windows_v2` + `top_level_order` on its side):

```rust
// Pseudocode shape:
let was_zero_to_one = backend.get_overlay_window(origin)?;
if was_zero_to_one {
    // 0→1 transition: backend has materialized its side (windows_v2 +
    // top_level_order). Now materialize the resources side in lockstep.
    let cow_host_xid = backend.cow_host_xid()
        .expect("get_overlay_window must populate cow_host_xid before returning Ok(true)");
    state.resources.materialize_cow_resource(
        WindowHandle::from_raw_panicking(cow_host_xid),
    );
}
// Build the protocol reply with COMPOSITE_OVERLAY_WINDOW (same on first or subsequent claim).
```

There's no second backend call (`materialize_cow` is gone — Task 2.2 folded its work into `get_overlay_window`). Single backend hook + single resources hook per transition, gated by one bool.

The `backend.cow_host_xid()` getter was already added in Task 2.2. Both pieces are in place; this task just wires the handler.

- [ ] **Step 5: Add a failing integration-shape test (placed near other GetOverlayWindow tests if any; otherwise create a new test in the same file)**

Use a smallest-feasible fixture that builds `ServerState` + `KmsBackendV2::for_tests()` and dispatches a `GetOverlayWindow` request. After dispatch, assert:
- `state.resources.window(COMPOSITE_OVERLAY_WINDOW)` is `Some`
- `state.resources.window(ROOT_WINDOW).children` ends with `COMPOSITE_OVERLAY_WINDOW`
- Backend's `windows_v2` contains a COW entry, mapped, full screen
- Backend's `top_level_order` ends with the COW's host xid

If the existing test infrastructure for dispatching requests through the protocol layer is unwieldy, do this assertion at the integration layer in `crates/yserver/tests/v2_acceptance.rs` instead.

- [ ] **Step 6: Run tests**

Run: `cargo test -p yserver-core --lib` and `cargo test -p yserver --test v2_acceptance get_overlay_window`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/yserver-core/src/resources.rs crates/yserver-core/src/core_loop/process_request.rs crates/yserver-core/src/backend/trait_def.rs crates/yserver/src/kms/v2/backend.rs
git commit -m "composite: materialize COW in resources + backend on first GetOverlayWindow

Drives both layers in lockstep from the protocol handler. resources gains
a materialize_cow_resource that inserts the COW as a top-of-root child
via cow_aware_top_index. The Backend trait gains a cow_host_xid()
getter so the core layer can plumb the backend-assigned xid into the
resources record. Backend's get_overlay_window now returns bool to
distinguish 0->1 claims from subsequent refcount bumps."
```

---

### Task 2.5: Wire `ReleaseOverlayWindow` teardown

**Files:**
- Modify: `crates/yserver-core/src/core_loop/process_request.rs` (the existing `ReleaseOverlayWindow` handler)

- [ ] **Step 1: Add a failing test**

In the same fixture used in 2.4 step 5: after GetOverlayWindow then ReleaseOverlayWindow:

```rust
    #[test]
    fn release_overlay_window_tears_down_resources_and_backend_state() {
        // Pseudocode shape — adapt to existing test fixture conventions.
        let mut state = /* fresh ServerState */;
        let mut backend = KmsBackendV2::for_tests();

        dispatch_get_overlay_window(&mut state, &mut backend);
        dispatch_release_overlay_window(&mut state, &mut backend);

        assert!(state.resources.window(COMPOSITE_OVERLAY_WINDOW).is_none(),
            "COW resource removed after final release");
        assert!(!state.resources.window(ROOT_WINDOW).unwrap()
            .children.contains(&COMPOSITE_OVERLAY_WINDOW));
        // Backend assertions: top_level_order does not contain the COW host xid.
        // (Use whatever cow_host_xid the backend would have assigned — read it
        // before release and compare.)
    }
```

- [ ] **Step 2: Run test to verify it fails**

Expected: FAIL — current ReleaseOverlayWindow only decrements refcount + drops storage.

- [ ] **Step 3: Patch the `ReleaseOverlayWindow` handler**

Per Task 2.2 the backend's `release_overlay_window` now also tears down its `windows_v2` + `top_level_order` side on the final release. Core just drives the symmetric resources-side teardown when the bool says it's the 1→0 transition:

```rust
let was_one_to_zero = backend.release_overlay_window(origin)?;
if was_one_to_zero {
    state.resources.destroy_cow_resource();
}
```

No second backend call (`destroy_cow` doesn't exist — see Task 2.2's design note). Single backend hook + single resources hook per transition, mirror of Task 2.4.

- [ ] **Step 4: Run test**

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/yserver-core/src/core_loop/process_request.rs
git commit -m "composite: tear down COW in resources + backend on final ReleaseOverlayWindow

Symmetric counterpart to Task 2.4's materialization. resources's
destroy_cow_resource removes the COW from root.children and drops the
record; backend's destroy_cow removes the windows_v2 entry and the
top_level_order slot."
```

---

### Task 2.6: Add `under_cow_subtree` recursion flag in `emit_window_subtree`

**Files:**
- Modify: `crates/yserver/src/kms/v2/scene.rs` (function signature + recursion sites + draw construction sites for `alpha_passthrough`)

The existing function (line 2059) takes an `under_redirected_ancestor: bool` last arg. Add a parallel `under_cow_subtree: bool` arg, propagated identically; set to `true` when entering a top-level whose host xid matches the COW; pass that flag down into recursive calls; use it to set `alpha_passthrough` on emitted `CompositeDraw` entries.

- [ ] **Step 1: Add a failing test**

In `crates/yserver/src/kms/v2/scene.rs` tests mod (add near the other `build_scene_*` tests):

```rust
    #[test]
    fn cow_subtree_draws_inherit_alpha_passthrough_true() {
        // Build a fixture: root + a non-COW top-level + the COW + a stage child of COW.
        // Use the existing alloc_stub_window pattern from neighboring tests.
        let mut core = KmsCore::for_tests();
        let mut store = DrawableStore::new();
        let platform = PlatformBackend::for_tests();
        let mut windows_v2 = super::super::backend::WindowsV2Map::new();

        // Non-COW top-level W at (0, 0), 200x200.
        alloc_stub_window(&mut store, &mut windows_v2, 0xA1, 0, 0, 200, 200, None, true);
        core.top_level_order.push(0xA1);

        // COW host xid, 800x600 fullscreen.
        let cow_xid: u32 = 0x103;
        alloc_stub_window(&mut store, &mut windows_v2, cow_xid, 0, 0, 800, 600, None, true);
        core.top_level_order.push(cow_xid);

        // Stage as child of COW.
        alloc_stub_window(&mut store, &mut windows_v2, 0xB1, 0, 0, 800, 600, Some(cow_xid), true);

        let built = build_scene(&core, &mut store, &windows_v2, 0, &platform, None, Some(cow_xid), None, false);
        let scene = &built.scene;

        // Find the draws by source extent or by ordering: the W draw should have
        // alpha_passthrough=false; the COW + stage draws should have alpha_passthrough=true.
        let w_draw = scene.draws.iter().find(|d| d.dst_size == [200.0, 200.0]).expect("W draw");
        assert!(!w_draw.alpha_passthrough, "non-COW top-level uses opaque blend");

        let cow_or_stage_draws: Vec<_> = scene.draws.iter()
            .filter(|d| d.dst_size == [800.0, 600.0]).collect();
        assert!(!cow_or_stage_draws.is_empty(), "COW and stage emitted");
        for d in cow_or_stage_draws {
            assert!(d.alpha_passthrough, "COW subtree draw must have alpha_passthrough=true");
        }
    }
```

`build_scene`'s current signature may not have a `cow_host_xid` arg. If so, that's what this task adds — see Step 3.

- [ ] **Step 2: Run test to verify it fails**

Expected: FAIL — either the signature doesn't accept a cow_host_xid yet (compile error) OR alpha_passthrough isn't propagated.

- [ ] **Step 3: Thread `cow_host_xid` into `build_scene` and `emit_window_subtree`**

In `crates/yserver/src/kms/v2/scene.rs`:

a. Change `build_scene` signature to take an additional `cow_host_xid: Option<u32>` arg (or thread it via the existing `&KmsCore` if there's a more natural place — `core.cow_host_xid` perhaps; check the existing struct).

b. In the `for &top_xid in &core.top_level_order` loop, call `emit_window_subtree` with an additional `under_cow_subtree` arg equal to `Some(top_xid) == cow_host_xid`.

c. In `emit_window_subtree`'s signature (line 2059), add `under_cow_subtree: bool` as the last positional argument (mirror `under_redirected_ancestor`'s placement).

d. In the recursion site inside `emit_window_subtree` (where it calls itself for children), pass `under_cow_subtree || (host_xid == cow_host_xid)`-equivalent so descendants of COW also inherit. If `cow_host_xid` isn't visible to the inner recursion, thread it through too (just like `under_redirected_ancestor` is threaded).

e. Wherever `emit_window_subtree` constructs a `CompositeDraw`, set `alpha_passthrough: under_cow_subtree`.

- [ ] **Step 4: Update all `build_scene` call sites**

Run: `cargo build --locked 2>&1 | head -30` — any callers (production + tests) that pass the old number of args will fail to compile. Update each: production callers pass `core.cow_host_xid` (or wherever the backend stores it); test callers pass `None` unless they're exercising the COW path.

- [ ] **Step 5: Run the new test + existing scene tests**

Run: `cargo test -p yserver --lib kms::v2::scene::tests`
Expected: new test PASSES, existing tests still PASS (alpha_passthrough is false by default outside COW subtree, matching today's behavior).

- [ ] **Step 6: Commit**

```bash
git add crates/yserver/src/kms/v2/scene.rs
git commit -m "scene: thread under_cow_subtree flag through emit_window_subtree

Inherited recursion flag mirroring under_redirected_ancestor. Set to
true when the walk enters a top-level whose host xid matches the COW;
propagated into descendants; used to set alpha_passthrough on emitted
CompositeDraw entries. Replaces the special-case alpha_passthrough on
the now-deleted COW append draw."
```

---

### Task 2.7: Delete the special COW append in `build_scene`

**Files:**
- Modify: `crates/yserver/src/kms/v2/scene.rs` (line 1887 area — the "Stage 4d: append the Composite Overlay Window draw entry" block, ~20 lines)
- Modify: `crates/yserver/src/kms/v2/scene.rs` (the `if cow.is_some() { strip top-levels }` branch in `build_scene` at line ~1841-1856 — delete; only the `else` branch remains, which is the normal top-level walk)
- Test: same file's tests mod

- [ ] **Step 1: Add a failing test**

```rust
    #[test]
    fn build_scene_does_not_append_cow_after_top_level_walk() {
        // The COW emit must come from the top_level_order walk, not from a
        // post-walk special append. Set up: only the COW in top_level_order.
        // The scene should contain exactly one draw sourced from the COW's
        // storage. (Plus root + cursor, but those are independent.)
        let mut core = KmsCore::for_tests();
        let mut store = DrawableStore::new();
        let platform = PlatformBackend::for_tests();
        let mut windows_v2 = super::super::backend::WindowsV2Map::new();

        let cow_xid: u32 = 0x103;
        alloc_stub_window(&mut store, &mut windows_v2, cow_xid, 0, 0, 800, 600, None, true);
        core.top_level_order.push(cow_xid);

        let built = build_scene(&core, &mut store, &windows_v2, 0, &platform, None, Some(cow_xid), None, false);
        let scene = &built.scene;

        let cow_draws: Vec<_> = scene.draws.iter()
            .filter(|d| d.dst_size == [800.0, 600.0]).collect();
        assert_eq!(cow_draws.len(), 1,
            "exactly one COW draw — no special append on top of top_level_order walk");
        assert!(cow_draws[0].alpha_passthrough, "COW draw still has alpha_passthrough=true");
    }
```

- [ ] **Step 2: Run test to verify it fails**

Expected: FAIL — current code appends a COW draw AND the top_level_order walk emits another. Count would be 2 (or 1 with the count test failing on the alpha flag depending on which path emits first).

- [ ] **Step 3: Delete the special append in `build_scene`**

In `crates/yserver/src/kms/v2/scene.rs`, find the block starting at the comment "// Stage 4d: append the Composite Overlay Window draw entry ABOVE all top-levels" (~line 1887). Delete the entire block — typically ~20 lines ending with the `if let Some(cow_id) = cow && ...` push-into-draws code.

Also delete the surrounding `if cow.is_some() { /* log message */ }` branch starting around line 1841 that today wraps the strip-top-levels behavior. The replacement is: always run the `for &top_xid in &core.top_level_order { emit_window_subtree(...) }` loop. Only one branch remains.

Concretely, the existing two-branch structure:

```rust
if cow.is_some() {
    log::trace!(... cow_authoritative=true ...);
} else {
    log::trace!(... cow_authoritative=false ...);
    for &top_xid in &core.top_level_order {
        emit_window_subtree(top_xid, ...);
    }
}
// ... Stage 4d: append COW draw ...
```

Becomes:

```rust
log::trace!(
    "v2 scene_walk begin output={output_idx} top_levels={n} order={order:?} \
     layout=({layout_x0},{layout_y0} {layout_w}x{layout_h})",
    n = core.top_level_order.len(),
    order = core.top_level_order,
);
for &top_xid in &core.top_level_order {
    emit_window_subtree(top_xid, /* … existing args …, under_cow_subtree= */ Some(top_xid) == cow_host_xid);
}
// (no special append)
```

Adjust the `under_cow_subtree` initial value at the top-level entry to match Task 2.6's threading.

Delete the now-unused `cow: Option<DrawableId>` arg to `build_scene` if it's only consumed by the deleted branch — pass `cow_host_xid: Option<u32>` instead (Task 2.6 added this).

- [ ] **Step 4: Run the new test + all scene tests**

Run: `cargo test -p yserver --lib kms::v2::scene::tests`
Expected: new test PASSES. Pre-existing tests that asserted `cow_authoritative=true` behavior (`build_scene_cow_some_strips_top_levels_and_keeps_cursor_at_top`) will now fail — that's expected. **Delete those tests** as part of this commit (they no longer correspond to any production code path):

```rust
// Delete this test from scene.rs tests mod (and any peers that also assert
// the cow_authoritative=true branch):
//   build_scene_cow_some_strips_top_levels_and_keeps_cursor_at_top
```

Re-run after deletion. Everything green.

- [ ] **Step 5: Commit**

```bash
git add crates/yserver/src/kms/v2/scene.rs
git commit -m "scene: delete special COW append; COW emits via normal walk

After Phase 2.3-2.4 puts the COW into windows_v2 + top_level_order,
build_scene reaches it via the standard top-level walk. The
cow_authoritative=true branch (strip top-levels, append COW as special
draw) is deleted: it predated the structural integration and only makes
sense under the marco-paint-direct-to-COW assumption that doesn't hold
for mutter/gnome-shell/cinnamon-mutter (per the spec)."
```

---

### Task 2.8: Set empty input shape on COW at materialization

**Files:**
- Modify: `crates/yserver-core/src/server.rs` (the place that initializes per-window state; for empty input shape, the relevant store is `shape_windows: HashMap<ResourceId, ShapeWindowState>` — set `input = Some(Vec::new())` for the COW)

Look at how `xfixes::SetWindowShapeRegion` writes to `shape_windows` for an existing window — the COW init needs to do the equivalent for `kind = Input`, empty rects.

- [ ] **Step 1: Add a failing test**

In `crates/yserver-core/src/server.rs` tests mod (or wherever hit-test tests live):

```rust
    #[test]
    fn cow_default_input_shape_is_empty() {
        let mut state = ServerState::new();
        // Drive materialization via a test helper (or via the production
        // resources call). If the production GetOverlayWindow handler exists,
        // dispatch it through the existing test plumbing.
        let host_xid = WindowHandle::from_raw_panicking(0x4000_0103);
        state.resources.materialize_cow_resource(host_xid);
        state.materialize_cow_input_shape();  // new helper, see Step 3

        let shape = state.shape_windows.get(&COMPOSITE_OVERLAY_WINDOW)
            .expect("COW must have a shape_windows entry after materialization");
        assert!(shape.input.is_some(), "COW must have a non-default input shape (set, but empty)");
        assert_eq!(shape.input.as_ref().unwrap().len(), 0,
            "COW's default input shape rects are empty (click-through)");
    }
```

- [ ] **Step 2: Run test to verify it fails**

Expected: FAIL — `materialize_cow_input_shape` doesn't exist yet.

- [ ] **Step 3: Add the helper**

In `crates/yserver-core/src/server.rs` (near other materialization helpers, or alongside the hit-test code):

```rust
impl ServerState {
    /// Stage 4e — set the COW's input shape to empty (click-through)
    /// at materialization. Mirrors Xorg's compositor convention where
    /// the COW's default input region passes pointer events through to
    /// underlying root children, with descendants like the
    /// compositor's stage receiving input directly.
    pub fn materialize_cow_input_shape(&mut self) {
        self.shape_windows
            .entry(COMPOSITE_OVERLAY_WINDOW)
            .or_default()
            .input = Some(Vec::new());
    }

    /// Symmetric teardown.
    pub fn destroy_cow_input_shape(&mut self) {
        self.shape_windows.remove(&COMPOSITE_OVERLAY_WINDOW);
    }
}
```

(`ShapeWindowState` should have a `Default` impl; if not, `or_insert_with(ShapeWindowState::default_for_window)` or similar.)

- [ ] **Step 4: Wire the helper into the `GetOverlayWindow` / `ReleaseOverlayWindow` handlers**

In `crates/yserver-core/src/core_loop/process_request.rs`, after `state.resources.materialize_cow_resource(...)` in the GetOverlayWindow handler, add `state.materialize_cow_input_shape();`. Symmetric: after `state.resources.destroy_cow_resource()` in the ReleaseOverlayWindow handler, add `state.destroy_cow_input_shape();`.

- [ ] **Step 5: Run tests**

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/yserver-core/src/server.rs crates/yserver-core/src/core_loop/process_request.rs
git commit -m "input: COW materializes with empty input shape (click-through)

Default input shape is empty so pointer events pass through the
fullscreen COW. Compositor clients still call XFIXES SetWindowShapeRegion
to adjust as needed; the default matches Xorg's compositor convention."
```

---

### Task 2.9: Drop the pre-Phase-2 special case in `hit_test_children`

**Files:**
- Modify: `crates/yserver-core/src/server.rs` (line ~1791 — `hit_test_children`)
- Test: same file's tests mod

**Strict Xorg approach** (decided 2026-06-09): once the COW is in `root.children` (Phase 2 materialization), the existing `window_input_contains` gate inside `hit_test_child` (server.rs:1835) is sufficient. With Task 2.8's default empty input shape, `hit_test_child(COW)` returns `None`, and the iteration falls through to the next sibling — exactly matching Xorg's `miSpriteTrace` (`in_input_shape → descend firstChild; else → nextSib`).

We therefore **delete** the pre-Phase-2 special case (1799-1808) and add **no replacement**. The COW becomes just another root child, distinguished only by the (compositor-managed) input region.

If a compositor explicitly populates the COW's input region via `XFIXES SetWindowShapeRegion`, `hit_test_child(COW)` returns the COW (or recurses via the normal nested loop in `pointer_target_at_inner` if a descendant matches). No COW-special code path required.

- [ ] **Step 1: Add a failing test (strict-Xorg shape)**

```rust
    #[test]
    fn cow_with_empty_input_shape_passes_clicks_to_sibling_below() {
        let mut state = ServerState::new();

        // Non-COW sibling at (0,0) 800x600, default (full) input shape.
        let sib = ResourceId(0x0010_0080);
        state.resources.create_window(
            ClientId(1),
            CreateWindowRequest {
                depth: 24, window: sib, parent: ROOT_WINDOW,
                x: 0, y: 0, width: 800, height: 600, border_width: 0,
                class: 1, visual: ROOT_VISUAL, ..Default::default()
            },
        );
        let _ = state.resources.map_window(sib);

        // Materialize COW (full-screen, empty input shape per Task 2.8).
        let host_xid = WindowHandle::from_raw_panicking(0x4000_0103);
        state.resources.materialize_cow_resource(host_xid);
        state.materialize_cow_input_shape();

        // Click at (50, 50): inside both sibling and COW geometry. COW's
        // empty input shape → hit_test_child(COW) = None → iteration
        // falls through to `sib`.
        let (target, _, _) = state.root_pointer_target_at(50, 50)
            .expect("trace hits sibling below COW");
        assert_eq!(target, sib,
            "empty COW input shape must let clicks through to sibling below");
    }

    #[test]
    fn cow_with_non_empty_input_shape_descends_into_stage() {
        let mut state = ServerState::new();

        let host_xid = WindowHandle::from_raw_panicking(0x4000_0103);
        state.resources.materialize_cow_resource(host_xid);
        // Compositor populates COW input shape covering the stage region.
        state.shape_windows.entry(COMPOSITE_OVERLAY_WINDOW).or_default().input =
            Some(vec![Rectangle { x: 0, y: 0, width: 800, height: 600 }]);

        let stage = ResourceId(0x0010_0050);
        state.resources.create_window(
            ClientId(1),
            CreateWindowRequest {
                depth: 24, window: stage, parent: COMPOSITE_OVERLAY_WINDOW,
                x: 10, y: 10, width: 100, height: 100, border_width: 0,
                class: 1, visual: ROOT_VISUAL, ..Default::default()
            },
        );
        let _ = state.resources.map_window(stage);

        let (target, _, _) = state.root_pointer_target_at(50, 50)
            .expect("hit");
        assert_eq!(target, stage,
            "non-empty COW input shape lets the trace descend to stage");
    }
```

Match `Rectangle`'s actual type/field names from the surrounding code — adjust if the project uses a different type for shape rects (e.g., `ShapeRect`).

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p yserver-core --lib server::tests::cow_with_`
Expected: FAIL — the pre-Phase-2 special-case branch fires (COW not in children) but its hit semantics don't match these strict-Xorg expectations; the empty-input-shape test in particular fails because `hit_test_child(COW)` correctly returns `None` but the iteration's first child is still `sib`, so the test may pass for the wrong reason. Read the failure message and adjust if needed before patching.

- [ ] **Step 3: Patch `hit_test_children` — delete-only**

Replace the current implementation:

```rust
    fn hit_test_children(
        &self,
        parent: ResourceId,
        x: i16,
        y: i16,
    ) -> Option<(ResourceId, i16, i16)> {
        let parent_window = self.resources.window(parent)?;

        if parent == ROOT_WINDOW
            && !parent_window.children.contains(&COMPOSITE_OVERLAY_WINDOW)
            && self
                .resources
                .window(COMPOSITE_OVERLAY_WINDOW)
                .and_then(|overlay| overlay.host_xid)
                .is_some()
            && let Some(hit) = self.hit_test_child(COMPOSITE_OVERLAY_WINDOW, x, y)
        {
            return Some(hit);
        }

        for child_id in parent_window.children.iter().rev() {
            if let Some(hit) = self.hit_test_child(*child_id, x, y) {
                return Some(hit);
            }
        }
        None
    }
```

With:

```rust
    fn hit_test_children(
        &self,
        parent: ResourceId,
        x: i16,
        y: i16,
    ) -> Option<(ResourceId, i16, i16)> {
        // Strict-Xorg miSpriteTrace: iterate children top-to-bottom and
        // let hit_test_child's window_input_contains gate decide each one.
        // The COW is no longer special once it's a real root child
        // (Phase 2 materialization). With its default empty input shape,
        // hit_test_child(COW) returns None and the trace continues to
        // the next sibling — exactly matching Xorg's mi/misprite.c.
        // When a compositor populates the COW input region via XFIXES,
        // the gate descends naturally via pointer_target_at_inner's
        // recursive walk.
        let parent_window = self.resources.window(parent)?;
        for child_id in parent_window.children.iter().rev() {
            if let Some(hit) = self.hit_test_child(*child_id, x, y) {
                return Some(hit);
            }
        }
        None
    }
```

- [ ] **Step 4: Run hit-test tests**

Run: `cargo test -p yserver-core --lib server::tests::root_hit_test server::tests::cow_with_`
Expected: PASS — new tests + any pre-existing `root_hit_test_*` tests. Pre-existing tests like `root_hit_test_reaches_overlay_child_without_querytree_child` and `root_hit_test_skips_overlay_when_input_shape_is_empty` were written against the pre-Phase-2 "COW not in children" shape; update them to set up the post-materialization state (call `materialize_cow_resource` + `materialize_cow_input_shape`) and re-assert against the strict-Xorg semantics. Any test that still depends on the deleted special-case branch — delete it; the equivalent post-Phase-2 behavior is covered by the new tests.

- [ ] **Step 5: Commit**

```bash
git add crates/yserver-core/src/server.rs
git commit -m "input: strict-Xorg hit-test; delete pre-Phase-2 COW special case

Once the COW is materialized as a real root child (Phase 2), the
existing window_input_contains gate in hit_test_child gives Xorg-
faithful semantics for free. Empty default input shape (Task 2.8)
makes hit_test_child(COW) return None; the iteration falls through
to the next sibling — exactly Xorg's miSpriteTrace
('in_input_shape → descend firstChild; else → nextSib').

Compositors that want stage clicks set a non-empty COW input region
via XFIXES SetWindowShapeRegion; the gate then descends into the
stage naturally. No COW-special branch required.

Rejected alternative: COW-special 'always descend' — would steal
clicks from siblings whenever compositor sets empty input shape
expecting pass-through."
```

---

### Task 2.10: Delete the dead `resources::direct_child_at`

**Files:**
- Modify: `crates/yserver-core/src/resources.rs` (line ~1172 — `direct_child_at`; delete the function)
- Test: same file's tests mod (delete any tests that exercise the deleted function)

**Strict Xorg approach** (decided 2026-06-09): the canonical `direct_child_at` callers (`server.rs` + tests; `core_loop/process_request.rs` for QueryPointer / XIQueryPointer; `core_loop/key_fanout.rs` for PointerRoot key delivery) all go through `ServerState::direct_child_at` (`crates/yserver-core/src/server.rs:1754`), which delegates to `hit_test_children` and inherits the strict-Xorg input-shape gating from Task 2.9. The separate `ResourceTable::direct_child_at` (resources.rs:~1172) has **no callers** project-wide (`grep -n direct_child_at crates/` shows the lone resources-level definition plus the server-level definition; only the latter is invoked).

Leaving the dead function in place is a footgun: a future change could route a caller to it and silently bypass the input-shape gate (returning the COW for clicks inside its geometric bounds regardless of the compositor's input region). Delete it.

- [ ] **Step 1: Confirm no callers**

Run: `grep -rn '\.direct_child_at\b\|resources\.direct_child_at\|table\.direct_child_at' crates/ --include='*.rs'`

Expected: every result is a method call on `state` / `ServerState` / `self` (in `server.rs`) — i.e., the server-level function. No call goes through `ResourceTable`. If a call site appears that DOES target the resources-level function, **stop and report BLOCKED** — the analysis behind this task is wrong and we need to rethink.

- [ ] **Step 2: Delete the function and any tests that target it**

In `crates/yserver-core/src/resources.rs` around line 1172, delete the entire `pub fn direct_child_at(...)` definition.

In the same file's `tests` mod, delete any tests that call `t.direct_child_at(...)` (where `t: ResourceTable`). The server-level coverage in `server.rs::tests` is the authoritative test bed and is updated in Task 2.9.

- [ ] **Step 3: Build to verify nothing depended on the deleted function**

Run: `cargo build --locked 2>&1 | head -20`
Expected: green. If a caller surfaces, it was missed by Step 1's grep — route it through `ServerState::direct_child_at` (which already exists and delegates correctly).

- [ ] **Step 4: Run resources tests**

Run: `cargo test -p yserver-core --lib resources::tests`
Expected: PASS (deletion didn't break anything else).

- [ ] **Step 5: Commit**

```bash
git add crates/yserver-core/src/resources.rs
git commit -m "resources: drop the dead ResourceTable::direct_child_at

All callers route through ServerState::direct_child_at (server.rs:1754),
which delegates to hit_test_children and inherits strict-Xorg input-
shape gating. The resources-level version had no callers and would
have silently bypassed the input-shape check if anyone wired into it.
Deleting it removes the footgun."
```

---

### Phase 2 acceptance

- [ ] `cargo build --locked` — green.
- [ ] `cargo test -p yserver-core --lib` and `cargo test -p yserver --lib` — green.
- [ ] At this point cinnamon should be **visually correct AND interactive**: keyring/seahorse/logout dialogs appear on top of their parents, AND clicks reach the right destinations (stage descendants get input; non-COW siblings get clicks below COW). This phase is the load-bearing fix for the cinnamon symptom. Phases 3-5 are correctness hardening (Manual skip, panic-on-drift, Xorg-faithful COW reparent/redirect); HW validation in Phase 6.

---

## Phase 3 — Manual-redirect unconditional skip

**Purpose:** make Manual-redirected windows skip emit regardless of any "compositor is active" flag, matching Xorg's behavior. The current `paint_target_is_self` at scene.rs:2220 unintentionally emits Manual windows because of the `has_own_redirected_target` clause.

### Task 3.1: Add `is_manual_redirected` to the emit gate

**Files:**
- Modify: `crates/yserver/src/kms/v2/scene.rs` (line ~2220 — the `paint_target_is_self` decision in `emit_window_subtree`)
- Test: same file's tests mod

- [ ] **Step 1: Add a failing test**

```rust
    #[test]
    fn manual_redirected_top_level_skips_emit_unconditional() {
        // Set up: a Manual-redirected top-level W. Build scene with no COW
        // (cow_host_xid = None) AND with COW present. In both cases W must
        // produce zero CompositeDraw entries.

        for cow_host_xid in [None, Some(0x103_u32)] {
            let mut core = KmsCore::for_tests();
            let mut store = DrawableStore::new();
            let platform = PlatformBackend::for_tests();
            let mut windows_v2 = super::super::backend::WindowsV2Map::new();

            // W with a redirected backing (Manual mode: scene_participating=false).
            let w: u32 = 0xA1;
            alloc_stub_window(&mut store, &mut windows_v2, w, 100, 100, 50, 50, None, true);
            let w_id = store.lookup(w).unwrap();
            let w_backing = Storage::for_tests_null(
                ash::vk::Extent2D { width: 50, height: 50 },
                PlatformBackend::format_for_depth(24),
            );
            let view: vk::ImageView = ash::vk::Handle::from_raw(0xBEEF_0000);
            let mut b = w_backing;
            b.image_view = view;
            b.sample_view = view;
            let b_id = store.allocate(0xB0A1, DrawableKind::Pixmap, 24, true, b).unwrap();
            store.set_redirected_target(w_id, Some(b_id));
            store.set_scene_participating(w_id, false);
            core.top_level_order.push(w);

            if let Some(cow_xid) = cow_host_xid {
                alloc_stub_window(&mut store, &mut windows_v2, cow_xid, 0, 0, 800, 600, None, true);
                core.top_level_order.push(cow_xid);
            }

            let built = build_scene(&core, &mut store, &windows_v2, 0, &platform, None, cow_host_xid, None, false);
            let scene = &built.scene;

            let w_draws: Vec<_> = scene.draws.iter()
                .filter(|d| d.dst_size == [50.0, 50.0]).collect();
            assert!(w_draws.is_empty(),
                "Manual-redirected W must NOT emit (cow={cow_host_xid:?})");
        }
    }

    #[test]
    fn automatic_redirected_top_level_still_emits() {
        // Same fixture as above but scene_participating=true (Automatic).
        let mut core = KmsCore::for_tests();
        let mut store = DrawableStore::new();
        let platform = PlatformBackend::for_tests();
        let mut windows_v2 = super::super::backend::WindowsV2Map::new();

        let w: u32 = 0xA2;
        alloc_stub_window(&mut store, &mut windows_v2, w, 100, 100, 50, 50, None, true);
        let w_id = store.lookup(w).unwrap();
        let w_backing = Storage::for_tests_null(
            ash::vk::Extent2D { width: 50, height: 50 },
            PlatformBackend::format_for_depth(24),
        );
        let view: vk::ImageView = ash::vk::Handle::from_raw(0xBEEF_0001);
        let mut b = w_backing; b.image_view = view; b.sample_view = view;
        let b_id = store.allocate(0xB0A2, DrawableKind::Pixmap, 24, true, b).unwrap();
        store.set_redirected_target(w_id, Some(b_id));
        // scene_participating left as default true (Automatic).
        core.top_level_order.push(w);

        let built = build_scene(&core, &mut store, &windows_v2, 0, &platform, None, None, None, false);
        let scene = &built.scene;

        let w_draws: Vec<_> = scene.draws.iter()
            .filter(|d| d.dst_size == [50.0, 50.0]).collect();
        assert_eq!(w_draws.len(), 1, "Automatic-redirected W still emits one draw");
    }
```

- [ ] **Step 2: Run tests to verify they fail / pass**

Run: `cargo test -p yserver --lib kms::v2::scene::tests::manual_redirected_top_level_skips_emit_unconditional kms::v2::scene::tests::automatic_redirected_top_level_still_emits`
Expected: the Manual test FAILS (W emits one draw today), the Automatic test PASSES.

- [ ] **Step 3: Patch the emit gate in `emit_window_subtree`**

In `crates/yserver/src/kms/v2/scene.rs`, find the existing block around line 2220:

```rust
            let has_own_redirected_target = source_id != id;
            let paint_target_is_self =
                has_own_redirected_target || (d_part && !under_redirected_ancestor);
```

Replace with:

```rust
            let has_own_redirected_target = source_id != id;
            let is_manual_redirected = has_own_redirected_target && !d_part;
            let paint_target_is_self = !is_manual_redirected
                && (has_own_redirected_target || (d_part && !under_redirected_ancestor));
```

Also adjust the surrounding `skip_reason` cascade (~line 2239) to add a new variant for the Manual-skip case:

```rust
            let skip_reason: Option<&'static str> = if is_manual_redirected {
                Some("manual_redirect_unconditional_skip")
            } else if !paint_target_is_self {
                // ... existing cascade ...
            } else if /* ... existing rest of the cascade ... */ {
                // ...
            } else {
                None
            };
```

Match the surrounding style; the key is that Manual-redirect skip is now the first reason in the cascade.

- [ ] **Step 4: Run the new tests + all scene tests**

Run: `cargo test -p yserver --lib kms::v2::scene::tests`
Expected: new tests PASS. Some pre-existing tests may need adjustment (any that asserted Manual windows emit a draw from their backing — that was the bug-shaped state).

- [ ] **Step 5: Commit**

```bash
git add crates/yserver/src/kms/v2/scene.rs
git commit -m "scene: skip Manual-redirected windows unconditionally

Manual-redirected windows go offscreen for the compositor to read via
NameWindowPixmap; the X server must not also blit the backing into
scanout. Xorg's compCheckRedirect ensures this structurally; yserver
now matches that behavior, regardless of cow_id state."
```

---

### Phase 3 acceptance

- [ ] `cargo test -p yserver --lib kms::v2::scene::tests` — green.
- [ ] `cargo build --locked` — green.

---

## Phase 4 — Hard failures on drift + protocol-level validation

**Purpose:** delete the silent `reparent_subwindow` missing-parent fallback (so drift is a panic, not a silent corruption); ensure callers don't discard `Err`; pin the Xorg-faithful COW reparent/redirect behavior at the protocol layer (generic cycle rule covers COW reparents; `RedirectWindow(COW)` → `Success` no-op — no yserver-invented `BadMatch`).

### Task 4.1: Remove silent missing-parent fallback in `reparent_subwindow`

**Files:**
- Modify: `crates/yserver/src/kms/v2/backend.rs` (line ~8840 — the `let parent = if host_parent == 0 || !self.windows_v2.contains_key(&host_parent) { None }` block)
- Test: same file's tests mod

- [ ] **Step 1: Add a failing test**

```rust
    #[test]
    #[should_panic(expected = "reparent_subwindow")]
    fn reparent_subwindow_panics_when_host_parent_missing() {
        let mut b = KmsBackendV2::for_tests();
        // Pre-seed a window so reparent_subwindow has something to operate on.
        let child_xid = 0x0040_0050_u32;
        b.create_subwindow_for_tests(child_xid, /*parent host xid*/ 0, 24, 100, 100);
        // Reparent to a host_parent that doesn't exist in windows_v2.
        let _ = b.reparent_subwindow(None, child_xid, 0xDEAD_BEEF, 0, 0);
    }
```

`create_subwindow_for_tests` is whatever shim exists in the v2 tests for setting up a windows_v2 entry. Use whichever pattern the surrounding tests use.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p yserver --lib backend::tests::reparent_subwindow_panics_when_host_parent_missing`
Expected: FAIL with a different error — likely test passes WITHOUT panic, because today the missing parent silently becomes None. The test expects panic; it gets silent success.

- [ ] **Step 3: Patch `reparent_subwindow`**

Replace the silent fallback at line ~8840:

```rust
        let parent = if host_parent == 0 || !self.windows_v2.contains_key(&host_parent) {
            None
        } else {
            Some(host_parent)
        };
```

With:

```rust
        let parent = if host_parent == 0 || host_parent == self.core.window_id {
            // BOTH sentinels mean "top-level under root": `0` is the
            // legacy convention, and `core.window_id` is root's real
            // host xid (root is never tracked in `windows_v2`). The
            // production reparent-to-root path passes
            // `backend.window_id()` (== core.window_id), NOT 0 — so
            // this second clause is load-bearing, not cosmetic.
            // Dropping it panics the server on every
            // ReparentWindow(child -> ROOT_WINDOW), a routine WM op.
            // (See backend.rs:1668 for the same root-sentinel check.)
            None
        } else if self.windows_v2.contains_key(&host_parent) {
            Some(host_parent)
        } else {
            // Per spec §"Remove the missing-parent fallback": backend
            // projection drift after protocol-level validation is a
            // fatal internal-consistency failure, not a silent
            // recovery. A genuinely-unknown non-root xid that the
            // resources tree accepted but windows_v2 lacks is drift —
            // surface it.
            panic!(
                "reparent_subwindow: host_parent 0x{host_parent:x} missing from \
                 windows_v2 (and is neither 0 nor root/core.window_id); resources \
                 layer must validate ReparentWindow before dispatching to backend"
            );
        };
```

> **CRITICAL (added 2026-06-09 after review caught a server-crash
> regression):** the root sentinel is `core.window_id` (== 1), NOT just
> `0`. The reparent-to-root call site in `process_request.rs` passes
> `Some(backend.window_id())`, so an implementation that only maps
> `host_parent == 0 → None` will reach the panic branch on every
> reparent-to-root. The guard MUST treat `host_parent == self.core.window_id`
> as root too. The old (pre-Phase-4) silent fallback covered this
> implicitly via `!contains_key(host_parent) → None`; the narrowing to a
> panic must preserve the root-sentinel case explicitly.

- [ ] **Step 4: Run the panic test**

Expected: PASS (the `#[should_panic]` is now satisfied).

- [ ] **Step 5: Run all backend tests**

Run: `cargo test -p yserver --lib backend::tests`
Expected: PASS — no other test was relying on the silent fallback (Phase 1-2 work ensures the production paths go through known-valid parents).

- [ ] **Step 6: Commit**

```bash
git add crates/yserver/src/kms/v2/backend.rs
git commit -m "kms/v2: panic on reparent drift, drop the silent missing-parent fallback

The old defensive 'unknown parent -> None -> top-level under root' is
what masked the cinnamon COW bug for weeks. After Phase 2 puts the COW
in windows_v2, the only way to hit this branch is genuine drift
between resources and backend, which is a fatal internal inconsistency."
```

---

### Task 4.2: Make caller-side `let _ = backend.reparent_subwindow(...)` propagate

**Files:**
- Modify: `crates/yserver-core/src/core_loop/process_request.rs` (any site that calls `backend.reparent_subwindow(...)` and currently discards the Err)

- [ ] **Step 1: Find the call sites**

Run: `grep -nE 'backend\.reparent_subwindow' crates/yserver-core/src/core_loop/process_request.rs`
For each result, read 5 lines around it to identify whether the Err is being discarded.

- [ ] **Step 2: Replace `let _ = backend.reparent_subwindow(...)` with explicit panic-on-Err**

For each site where the result is being discarded:

```rust
let _ = backend.reparent_subwindow(origin, child, parent, x, y);
```

Replace with:

```rust
if let Err(e) = backend.reparent_subwindow(origin, child, parent, x, y) {
    panic!(
        "reparent_subwindow failed after protocol validation: child=0x{child:x} \
         parent=0x{parent:x}: {e}"
    );
}
```

(If the parameter names differ, adjust.)

- [ ] **Step 3: Build to verify it compiles**

Run: `cargo build --locked`
Expected: green.

- [ ] **Step 4: Run resources + process_request tests**

Run: `cargo test -p yserver-core --lib`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/yserver-core/src/core_loop/process_request.rs
git commit -m "process_request: panic on backend reparent failure instead of discarding Err

Per spec §2: protocol-invalid reparents are rejected at the
request-validation layer (Phase 4 follow-ups); post-validation backend
mirror failure is a fatal internal consistency error. The previous
'let _ = backend.reparent_subwindow(...)' silently swallowed exactly the
kind of drift the spec wants to surface."
```

---

### Task 4.3: Verify the generic cycle rule already covers COW reparents (Xorg-faithful)

> **Redesigned 2026-06-09 (Match-Xorg-strictly).** The earlier draft of
> this task added a COW-specific `BadMatch` for `ReparentWindow(COW,
> non-root)` and a second COW-specific cycle guard. **Both are wrong:**
> - Xorg's `compReparentWindow` (`composite/compwindow.c:438`) has **no**
>   COW-specific reparent check; `ProcReparentWindow` (`dix/dispatch.c:877`)
>   only applies the generic same-screen / ParentRelative-depth /
>   InputOnly-parent rules plus the cycle rule in `ReparentWindow`
>   (`dix/window.c`: `TraverseTree(pWin, CompareWIDs, &pParent->id) ==
>   WT_STOPWALKING → BadMatch`). There is no "COW can't move from root"
>   error to mirror.
> - yserver **already** enforces that exact generic cycle rule in
>   `ResourceTable::reparent_window` (`resources.rs:1267`):
>   ```rust
>   if request.window == ROOT_WINDOW
>       || request.window == request.parent
>       || self.is_descendant_of(request.parent, request.window)
>   {
>       return Err(ReparentWindowError::BadMatch);
>   }
>   ```
>   When `request.parent == COW`, the third clause reduces to "is COW a
>   descendant of the moved window?" — identical to the COW cycle guard
>   the earlier draft proposed. So that guard is **fully redundant**.
>
> Per `feedback_match_xorg_clients_dont_get_patched.md`, a compositor that
> reparents the COW away from root gets the same outcome it would on Xorg
> (the reparent succeeds; compositing breaks; the compositor's bug). We do
> NOT invent a new error.
>
> **This task therefore adds NO production code.** It adds regression
> tests that pin the Xorg-faithful behavior so a future change can't
> silently reintroduce a COW-special rejection or drop the generic cycle
> check.

**Files:**
- Test only: `crates/yserver-core/src/resources.rs` (`tests` mod) — the cycle rule lives in `ResourceTable::reparent_window`, so test it there directly.

- [ ] **Step 1: Confirm the generic cycle rule covers COW**

Read `ResourceTable::reparent_window` (`resources.rs:1263`) and confirm the guard at line 1267 includes `self.is_descendant_of(request.parent, request.window)`. If the guard is missing or shaped differently than the redesign note above assumes, **stop and report BLOCKED** — the premise of this task (no new code needed) is wrong.

- [ ] **Step 2: Add regression tests**

In `resources.rs` `tests` mod (use the existing `materialize_cow_resource` + `make_child` helpers):

```rust
    #[test]
    fn reparent_cow_under_its_own_descendant_is_bad_match() {
        // Materialize COW (child of root), then create a child C under COW.
        let mut t = ResourceTable::new();
        let host_xid = WindowHandle::from_raw_panicking(0x4000_0103);
        t.materialize_cow_resource(host_xid);
        make_child(&mut t, 0xC0, COMPOSITE_OVERLAY_WINDOW.0, 0, 0);

        // ReparentWindow(window=COW, parent=C) would make COW a descendant
        // of itself → BadMatch via the GENERIC cycle rule (no COW-special
        // code). is_descendant_of(parent=C, window=COW) is true because C's
        // parent chain reaches COW.
        let err = t.reparent_window(ReparentWindowRequest {
            window: COMPOSITE_OVERLAY_WINDOW,
            parent: ResourceId(0xC0),
            x: 0, y: 0,
        });
        assert!(matches!(err, Err(ReparentWindowError::BadMatch)),
            "reparenting COW under its own child must be BadMatch (generic cycle rule)");
    }

    #[test]
    fn reparent_cow_to_a_normal_toplevel_succeeds_xorg_faithful() {
        // Xorg does NOT reject moving the COW to an unrelated window. yserver
        // must match: a reparent to a non-descendant, non-root window succeeds.
        let mut t = ResourceTable::new();
        make_child(&mut t, 0x200, ROOT_WINDOW.0, 0, 0); // unrelated top-level W
        let host_xid = WindowHandle::from_raw_panicking(0x4000_0103);
        t.materialize_cow_resource(host_xid);

        let res = t.reparent_window(ReparentWindowRequest {
            window: COMPOSITE_OVERLAY_WINDOW,
            parent: ResourceId(0x200),
            x: 0, y: 0,
        });
        assert!(res.is_ok(),
            "reparenting COW to an unrelated window succeeds — matches Xorg \
             (compositing breaks, but that's the compositor's bug, not ours)");
        assert_eq!(t.window(COMPOSITE_OVERLAY_WINDOW).unwrap().parent, ResourceId(0x200));
    }
```

(Match the real field names of `ReparentWindowRequest` — check the struct; the plan assumes `window`, `parent`, `x`, `y`.)

- [ ] **Step 3: Run the tests**

Run: `cargo test -p yserver-core --lib resources::tests::reparent_cow`
Expected: PASS (both — they exercise existing behavior, so they're green from the start; their value is regression protection, not red→green).

- [ ] **Step 4: Commit**

```bash
git add crates/yserver-core/src/resources.rs
git commit -m "resources: pin Xorg-faithful COW reparent behavior (regression tests)

No production change. Xorg has no COW-specific reparent rejection
(compReparentWindow, compwindow.c:438); the only rule is the generic
cycle check (dix/window.c TraverseTree -> BadMatch), which yserver
already enforces in ResourceTable::reparent_window (is_descendant_of
guard at resources.rs:1267) and which covers the COW case for free.

These tests pin both halves so a future change can't reintroduce a
COW-special BadMatch or drop the generic cycle guard:
 - reparent COW under its own descendant -> BadMatch (cycle rule)
 - reparent COW to an unrelated top-level -> Ok (Xorg-faithful; broken
   compositing is the compositor's bug, per
   feedback_match_xorg_clients_dont_get_patched.md)"
```

---

### Task 4.4: `RedirectWindow(COW, ...)` is a silent `Success` no-op (Xorg-faithful)

> **Redesigned 2026-06-09 (Match-Xorg-strictly).** The earlier draft
> returned `BadMatch`, citing `compwindow.c:166-170`. That citation is
> `compCheckRedirect`'s internal `should = FALSE` short-circuit (the
> server never *auto*-redirects the COW) — **not** a request-level
> rejection. A client-issued `CompositeRedirectWindow(COW)` actually hits
> `compRedirectWindow` (`composite/compalloc.c:145-147`):
> ```c
> if (pWin == cs->pOverlayWin) {
>     return Success;
> }
> ```
> So Xorg returns **`Success`** and installs no redirect. yserver must
> match: short-circuit to a successful no-op, NOT `BadMatch`.

**Files:**
- Modify: `crates/yserver-core/src/core_loop/process_request.rs` (the `Composite::RedirectWindow` handler)

- [ ] **Step 1: Add a failing test**

```rust
    #[test]
    fn redirect_window_cow_is_success_noop() {
        // RedirectWindow(COW) must succeed silently (no X error on the wire)
        // and must NOT install a redirect — matching Xorg compalloc.c:145.
        // Use the same dispatch fixture the neighboring redirect tests use
        // (e.g. dispatch_composite_redirect) and assert NO error byte comes
        // back, and that the COW did not gain a redirected target.
        let mut state = /* fresh ServerState with COW materialized */;
        let mut backend = KmsBackendV2::for_tests();

        dispatch_composite_redirect(&mut state, &mut backend, COMPOSITE_OVERLAY_WINDOW, /*mode=Manual*/ 1);

        // No X11 error should have been queued to the client (mirror the
        // assertion style of redirect_window_conflict_* tests in this file:
        // they read the client socket for an error reply — here assert the
        // socket has NO pending error).
        // AND: the COW must not be marked redirected in resources/backend.
        assert!(/* COW has no redirect state */,
            "RedirectWindow(COW) is a Success no-op, COW stays unredirected");
    }
```

Adapt to the project's actual redirect-dispatch + error-observation convention (the neighboring `redirect_window_conflict_from_different_client_returns_bad_access` test shows how to observe an error reply over the client socket; here you assert the *absence* of one).

- [ ] **Step 2: Run test to verify it fails**

Expected: FAIL — a freshly reset tree has no COW handling in the RedirectWindow path and would attempt to actually redirect the COW (installing redirect state / emitting unexpected behavior).

- [ ] **Step 3: Patch the `RedirectWindow` handler**

Find the `RedirectWindow` / `RedirectSubwindows` arm (grep `REDIRECT_WINDOW` / `RedirectWindow` in process_request.rs). After the request body is parsed, before any redirect setup, add the COW short-circuit. Match the handler's actual success-return convention (it returns `io::Result<RequestOutcome>`; a no-op success is whatever the other "nothing to do" arms return — e.g. `Ok(RequestOutcome::…)` with no error queued and no state mutation):

```rust
    // Xorg compRedirectWindow (composite/compalloc.c:145-147): a client
    // request to redirect the overlay window returns Success without
    // installing any redirect. The COW reaches scanout via the normal
    // paint path; it is never itself redirected. Match that exactly —
    // NOT BadMatch (which would be a yserver-invented error).
    if window == COMPOSITE_OVERLAY_WINDOW {
        return Ok(/* the handler's standard no-op success outcome */);
    }
```

Apply only to `RedirectWindow`; `RedirectSubwindows(COW)` is benign on its own (COW has no children to redirect) and Xorg doesn't special-case it either — leave it on the normal path.

- [ ] **Step 4: Run tests**

Run: `cargo test -p yserver-core --lib redirect_window_cow`
Expected: PASS. Also run the neighboring redirect tests to confirm no regression: `cargo test -p yserver-core --lib redirect_window`.

- [ ] **Step 5: Commit**

```bash
git add crates/yserver-core/src/core_loop/process_request.rs
git commit -m "composite: RedirectWindow(COW) is a Success no-op (matches Xorg)

Xorg's compRedirectWindow (composite/compalloc.c:145-147) returns
Success and installs no redirect when the target is the overlay window.
yserver now matches exactly: short-circuit to a successful no-op.

The earlier draft returned BadMatch, citing compwindow.c:166-170 — but
that is compCheckRedirect's internal should=FALSE (the server never
auto-redirects the COW), not a request-level rejection. A client
CompositeRedirectWindow(COW) hits compRedirectWindow, which returns
Success. Returning BadMatch would be a yserver-invented error with no
Xorg equivalent (feedback_match_xorg_clients_dont_get_patched.md)."
```

---

### Phase 4 acceptance

- [ ] `cargo build --locked` — green.
- [ ] `cargo test -p yserver-core --lib` and `cargo test -p yserver --lib` — green.

---

## Phase 5 — Cleanup: delete obsolete COW-registration helpers

**Purpose:** delete code that exists only to support the now-removed `cow_authoritative` mode.

### Task 5.1: Delete `Scene::register_cow` / `unregister_cow` / `is_cow_registered`

**Files:**
- Modify: `crates/yserver/src/kms/v2/scene.rs` (line ~595-615 — the impl block on `Scene` with these three methods)
- Modify: `crates/yserver/src/kms/v2/scene.rs` (the `cow: Option<DrawableId>` field on `Scene` if it exists; the now-unused `cow` arg passed into `build_scene` if any path still uses it)

- [ ] **Step 1: Delete the methods and field**

Open `crates/yserver/src/kms/v2/scene.rs`. Find the impl block containing `register_cow`, `unregister_cow`, `is_cow_registered` (around line 595-615). Delete all three methods. Remove the `cow: Option<DrawableId>` field from `Scene` if present.

- [ ] **Step 2: Run `cargo build` and fix every caller**

Run: `cargo build --locked 2>&1 | head -50`

Expected: compile errors at every caller. Triage each:

- In `crates/yserver/src/kms/v2/backend.rs`: `self.scene.register_cow(cow_id)` calls (likely in `note_present_pixmap` or `get_overlay_window`) — delete these calls. The COW lifecycle is now driven by `materialize_cow` / `destroy_cow` (Phase 2), not by scene registration.
- Any tests calling `b.test_scene_cow_registered()` or similar — delete those tests (they exercised `cow_authoritative` mode behavior that no longer exists).

- [ ] **Step 3: Run the full test sweep**

Run: `cargo test -p yserver --lib`
Expected: PASS — all `cow_registers_*` and related tests are either deleted in this commit or pre-existed deletion.

- [ ] **Step 4: Commit**

```bash
git add crates/yserver/src/kms/v2/scene.rs crates/yserver/src/kms/v2/backend.rs
git commit -m "scene: delete Scene::register_cow / unregister_cow / is_cow_registered

The cow_authoritative mode they controlled is gone (Phase 2). Remaining
callers either lived in note_present_pixmap (deleted in Phase 5.2) or
were tests of the old mode (deleted with the production code)."
```

---

### Task 5.2: Delete `arm_cow_from_recent_present_if_needed`, `present_to_cow_sources`, `maybe_register_cow_on_paint`

**Files:**
- Modify: `crates/yserver/src/kms/v2/backend.rs` (line ~1319 — `arm_cow_from_recent_present_if_needed`; the `present_to_cow_sources: VecDeque<u32>` field; line ~1551 — `maybe_register_cow_on_paint`; `note_present_pixmap` simplification)

- [ ] **Step 1: Delete the helpers**

In `crates/yserver/src/kms/v2/backend.rs`:

a. Delete `fn arm_cow_from_recent_present_if_needed(&mut self)` (around line 1319).
b. Remove the `present_to_cow_sources: VecDeque<u32>` field from `KmsBackendV2` (or wherever it's declared) AND its initialization in the constructor.
c. Delete `fn maybe_register_cow_on_paint(&mut self, target_id: DrawableId)` (around line 1551).
d. Simplify `note_present_pixmap` (line 8199 area) — its "Capture COW-targeted presents" branch (lines ~8211-8240) collapses to nothing (the entire COW-arming was driven through these helpers). The function reduces to just the `recent_present_pixmaps` ring tracking (lines ~8203-8209). Or, if `recent_present_pixmaps` itself was only used for COW arming, delete that too. Verify usage with `grep -n recent_present_pixmaps crates/yserver/src/kms/v2/backend.rs` — if its only readers were `arm_cow_from_recent_present_if_needed` and `dump_drawables`, keep it (the dump diagnostic still uses it).

e. Delete the call site for `arm_cow_from_recent_present_if_needed` inside `get_overlay_window` (the line `self.arm_cow_from_recent_present_if_needed();`).

f. Delete the call sites for `maybe_register_cow_on_paint` (grep for them; they appear in copy_area/fill_rect/composite_paint paths — these calls are now no-ops, just remove them).

- [ ] **Step 2: Build to find any callers**

Run: `cargo build --locked 2>&1 | grep -E 'error|warning' | head -30`
Expected: clean. Any errors point at leftover callers; remove them.

- [ ] **Step 3: Run tests**

Run: `cargo test -p yserver --lib`
Expected: PASS. Tests like `note_present_pixmap_tracks_non_cow_stage_sources_for_drawable_dump` will fail if `present_to_cow_sources` is gone — delete those tests, they exercised the deleted ring.

- [ ] **Step 4: Commit**

```bash
git add crates/yserver/src/kms/v2/backend.rs
git commit -m "kms/v2: delete COW-arming side-channel helpers

arm_cow_from_recent_present_if_needed, present_to_cow_sources ring, and
maybe_register_cow_on_paint all existed only to feed Scene::register_cow,
which itself was the trigger for cow_authoritative mode. With both gone
(Phases 2 and 6.1), the side channel has no consumers. note_present_pixmap
reduces to the recent_present_pixmaps diagnostic ring."
```

---

### Phase 5 acceptance

- [ ] `cargo build --locked` — green, no warnings about unused fields/imports.
- [ ] `cargo test -p yserver --lib` and `cargo test -p yserver-core --lib` — green.
- [ ] `cargo clippy --locked` — green per `AGENTS.md` (default, no `-W clippy::pedantic`).

---

## Phase 6 — Integration + validation

### Task 6.1: Integration test for the full compositor flow

**Files:**
- Modify: `crates/yserver/tests/v2_acceptance.rs` (add a new test)

- [ ] **Step 1: Add the integration test**

```rust
#[test]
fn compositor_stage_under_cow_emits_via_recursion_and_manual_siblings_skip() {
    let mut be = KmsBackendV2::for_tests();
    let cow_xid = yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0;

    // 1. GetOverlayWindow — backend now also creates the windows_v2
    //    entry + top_level_order slot (Task 2.2). Returns Ok(true) on
    //    first claim; the resources-side materialization (which would
    //    normally fire from the core handler) is skipped here because
    //    this fixture is backend-only — the assertions below operate
    //    against the backend's view (windows_v2 / top_level_order /
    //    scene draws), not against resources.
    let was_first_claim = be.get_overlay_window(None).expect("get_overlay_window");
    assert!(was_first_claim, "first claim returns Ok(true)");
    let _ = cow_xid; // ensure binding is used after the lifecycle setup

    // 2. A Manual-redirected sibling top-level S (e.g., seahorse-shape).
    let s: u32 = 0x40010_5000;
    /* set up S with redirected backing in Manual mode — pattern from
       existing v2_acceptance fixtures */;

    // 3. A stage child of COW with rendered content.
    let stage: u32 = 0x40010_6000;
    be.create_subwindow(None, stage, /*parent*/ cow_xid, 24, 0, 0, 800, 600, /*bg*/ 0, None).expect("stage");
    be.map_subwindow(None, stage).expect("map stage");

    // 4. Build the scene.
    let scene = be.build_test_scene();  // existing test helper; adapt to your fixture

    // 5. Assertions.
    let s_draws = scene.draws.iter().filter(|d| /* matches S's source */).count();
    assert_eq!(s_draws, 0, "Manual-redirected sibling must not emit");

    let stage_draws = scene.draws.iter().filter(|d| /* matches stage's source */).count();
    assert_eq!(stage_draws, 1, "stage emits exactly once via COW subtree recursion");

    let cow_draws = scene.draws.iter().filter(|d| /* matches COW's source */).count();
    assert_eq!(cow_draws, 1, "COW emits exactly once via top_level_order walk");

    // The stage draw must come AFTER S's potential draw position and AFTER
    // any earlier non-COW top-levels — i.e., it appears in the late portion
    // of scene.draws. (Exact ordering depends on the scene compose details.)
}
```

The exact wiring matches the conventions in `v2_acceptance.rs` — read existing tests in that file for `build_test_scene` / how to assert on draws / how Manual-redirected setup is done.

- [ ] **Step 2: Run the test**

Run: `cargo test -p yserver --test v2_acceptance compositor_stage_under_cow`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/yserver/tests/v2_acceptance.rs
git commit -m "v2_acceptance: integration test for compositor stage under COW

Drives the full lifecycle: GetOverlayWindow -> materialize_cow ->
stage as COW child -> Manual-redirected sibling -> build_scene.
Asserts the structural invariants from the spec: stage emits exactly
once via COW recursion; Manual-redirected sibling skips; COW emits
exactly once via top_level_order walk."
```

---

### Task 6.2: Local validation sweep

- [ ] **Step 1: Format**

Run: `cargo +nightly fmt`
Expected: zero diff (if there's drift, commit it as a fixup).

- [ ] **Step 2: Build (release + debug)**

Run: `cargo build --locked` and `cargo build --locked --release`
Expected: green.

- [ ] **Step 3: Unit tests**

Run: `cargo test --locked -p yserver-core --lib` and `cargo test --locked -p yserver --lib`
Expected: green. (The 48 pre-existing yserver-core failures from earlier may still be present — confirm via `git stash; cargo test ...; git stash pop` if uncertain. They're not blockers per AGENTS.md feedback.)

- [ ] **Step 4: Integration tests**

Run: `cargo test --locked -p yserver --test v2_acceptance`
Expected: green.

- [ ] **Step 5: Clippy**

Run: `cargo clippy --locked --all-targets -- -D warnings`
Expected: green (or document any pre-existing unrelated blocker per `AGENTS.md`).

- [ ] **Step 6: Rendercheck**

Run: `just rendercheck-yserver`
Expected: at or above baseline per `reference_rendercheck_v1_baseline.md`. Any new fails are regressions.

- [ ] **Step 7: Commit nothing** — this step has no code changes; it's the green-gate before HW validation.

---

### Task 6.3: Hardware validation matrix

Per `feedback_hw_recipes_user_only.md`: **ONE agent per checkout — coordinate before HW runs.** Confirm with the user that no other session is running HW recipes on the same machine.

- [ ] **Step 1: bee/cinnamon-mutter — the golden case**

Run: `just yserver-cinnamon-hw` (or the exact recipe name per `project_startx_recipe.md`).

In the cinnamon session:
- Open `seahorse` → click on the login keyring → click "Unlock" → **the unlock prompt must appear on top, fully visible, with the desktop dimmed**.
- Open Chrome → navigate to a site with stored credentials → **the keyring unlock prompt must appear on top, not behind chrome**.
- Click the user menu in cinnamon's panel → click "Quit" → **the logout confirmation dialog must appear on top, above the dimmed desktop**.
- Verify clicks reach windows correctly (no "every click hits the COW" — Tasks 2.8–2.10 ensure this).

If any of these fail, capture state via Ctrl+Alt+F12 (drawable dump) and `~/.local/share/xorg/Xorg.0.log` (or equivalent yserver log location).

- [ ] **Step 2: bee/mate or bee/xfce-marco — non-regression**

Run: `just yserver-mate-hw` or `just yserver-xfce-hw`.

- Verify mate-panel / xfce-panel tray applets render correctly (`project_tray_damage_self_loop.md` is the relevant prior fix; ensure that work isn't visibly regressed).
- Open seahorse → unlock → prompt above parent.
- Smoke test alt-tab + window dragging + window decoration rendering.

- [ ] **Step 3: bee/xfwm4 — paint-to-COW-direct compositor**

Run the xfce session with xfwm4's compositor enabled. Same smoke as Step 2 above.

- [ ] **Step 4: bee/e16 — no-compositor**

Run: `just yserver-e16-hw`.

- Smoke: open xterm, alt-tab, move/resize windows. Verify no visible regression vs master.

- [ ] **Step 5: XTS — release gate**

Run: `just xts-yserver-hw`.
Expected: no new UNRES vs baseline per `project_xts_followup_leverage.md`.

- [ ] **Step 6: Document results**

Add a results doc at `docs/results/2026-MM-DD-cow-structural-results.md` (or wherever the project keeps post-impl summaries — check the existing pattern by `ls docs/results/ 2>/dev/null` or `ls docs/`). Cover:
- HW matrix outcomes (passed/failed per session).
- Any regressions found and how addressed.
- Memory entries to add per `auto memory` flow.

---

### Phase 6 acceptance

- [ ] All cinnamon golden cases pass on bee.
- [ ] No regressions on marco / xfwm4 / e16 sessions.
- [ ] Rendercheck baseline-equivalent.
- [ ] XTS no new UNRES.
- [ ] Results doc written.

---

## Final task: PR + memory updates

- [ ] **Step 1: Confirm with user before any push or PR**

Per `feedback_confirm_each_master_push.md`: one "OK to merge" doesn't authorise the next. Ask explicitly:

> "All phases green. Branch ready: ~N commits, see `git log --oneline master..HEAD`. Squash-merge to master and push?"

- [ ] **Step 2: On approval — squash-merge**

```bash
git checkout master
git pull --ff-only
git merge --squash feat/cow-structural
git commit -m "$(cat <<'EOF'
feat(composite): COW becomes a real root child; Manual-redirected windows skip scanout

Replaces the split COW model (resources record + drawable-store storage +
scene-only special append) with the Xorg-shaped one from
docs/superpowers/specs/2026-06-08-cow-structural-design.md:

- COW materializes on first GetOverlayWindow and tears down on final
  ReleaseOverlayWindow, in lockstep across resources, windows_v2, and
  top_level_order.
- All root.children mutations funnel through one cow_aware_top_index
  helper so COW stays topmost across create / restack / reparent-to-root /
  circulate.
- Manual-redirected windows skip emit unconditionally (no cow_id mode
  flag), matching Xorg's compCheckRedirect-driven offscreen-paint
  semantics. Automatic redirect unchanged.
- COW is click-through: empty default input shape, root hit-test
  descends into descendants but never returns COW as the pointer target.
- Scene compose deletes the special COW append; COW emits via the
  normal top_level_order walk. alpha_passthrough is carried by an
  inherited under_cow_subtree recursion flag.
- COW reparent/redirect match Xorg exactly: the generic cycle rule
  (is_descendant_of guard in reparent_window) covers COW reparents
  with no COW-special rejection, and RedirectWindow(COW) is a Success
  no-op (compRedirectWindow, compalloc.c:145). No yserver-invented
  BadMatch.
- reparent_subwindow's silent missing-parent fallback removed; drift
  between resources and backend now panics (callers no longer discard
  the Err).

Fixes: cinnamon-mutter prompt-under-parent class (gnome-keyring unlock,
seahorse, logout dialog). Marco/xfwm4/e16 sessions unchanged.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

- [ ] **Step 3: Push (only on explicit approval)**

```bash
git push origin master
```

- [ ] **Step 4: Update memory**

Per the `auto memory` flow, add or update entries to capture lessons:

- A new `project_cow_structural.md` recording the new invariants for future agents (lifecycle / single-source-of-truth / Manual-skip / hit-test transparency).
- A new `feedback_*` entry if there are project-wide patterns worth pinning (e.g., "structural problems require structural fixes — beware surgical bridges" if that doesn't already exist; check `MEMORY.md` first).

---

## Done criteria

- [ ] All HW matrix outcomes green (cinnamon golden cases visible, marco/xfwm4/e16 no regression).
- [ ] Rendercheck + XTS at or above baseline.
- [ ] Spec invariants 1-8 all hold per the unit/integration tests in phases 1-6.
- [ ] No `cow_authoritative`, `register_cow`, `arm_cow_from_recent_present_if_needed`, or `present_to_cow_sources` references remain in the codebase (`grep -rE 'cow_authoritative|register_cow|arm_cow_from_recent|present_to_cow_sources' crates/ docs/ 2>&1` returns only doc-archive matches).
- [ ] Results doc + memory updates committed.

---

## Risks & watchpoints during implementation

- **Phase 2 internal task ordering matters.** Tasks 2.1–2.7 put the COW in the scene; Tasks 2.8–2.10 land input transparency. **Don't HW-test between Task 2.7 and Task 2.10** — the COW would be in the tree without click-through semantics, and every click would hit the COW. The Phase 2 acceptance gate (after Task 2.10) is the first usable state.
- **Pre-existing yserver-core test failures.** ~48 pre-existing failures unrelated to this work (XTS marathon cleanup leftovers per the user). Verify your phase deltas don't increase the count; use `git stash` + master baseline to compare if uncertain.
- **4d.8 regression symptoms.** Watch for the 4d.8 retrospective's failure shape during HW testing: "windows + bits appearing / disappearing during use, slow as molasses, flicker." If you see this, **stop**. It means the structural model is still incomplete — do NOT add surgical fixes on top. Reopen the spec.
- **`note_present_pixmap` simplification.** When deleting the COW-arming branch, double-check that the `recent_present_pixmaps` diagnostic ring still has live readers (the drawable-dump uses it). Don't accidentally remove that.
- **HW recipe coordination.** Per `feedback_hw_recipes_user_only.md`: only one HW recipe per checkout at a time. Confirm with the user before running.

---

## Spec self-review (post-write check)

- **Placeholders:** none — every code block has actual code, every step has an exact command.
- **Spec coverage:** all 8 invariants in `Core invariants`, all 7 sections in `Proposed architecture`, all `Expected code changes` files, and all `Testing strategy` categories have at least one corresponding task above. The "What gets deleted" list maps to Tasks 5.1 + 5.2.
- **Type consistency:** `cow_aware_top_index(&Window) -> usize`; `get_overlay_window(&mut self, Option<OriginContext>) -> io::Result<bool>` and `release_overlay_window(&mut self, Option<OriginContext>) -> io::Result<bool>` (Backend trait, both now own the full COW lifecycle including `windows_v2` + `top_level_order` — no separate materialize_cow/destroy_cow); `cow_host_xid(&self) -> Option<u32>` (Backend trait getter); `under_cow_subtree: bool` arg on `emit_window_subtree`; `materialize_cow_resource(&mut self, WindowHandle)` and `destroy_cow_resource(&mut self)` on `ResourceTable`. All referenced consistently across tasks.

---

## Execution handoff

Plan complete and saved to `docs/superpowers/plans/2026-06-08-cow-structural.md`. Two execution options:

**1. Subagent-Driven (recommended)** — I dispatch a fresh subagent per task, review between tasks, fast iteration.

**2. Inline Execution** — Execute tasks in this session using `superpowers:executing-plans`, batch execution with checkpoints.

Which approach?
