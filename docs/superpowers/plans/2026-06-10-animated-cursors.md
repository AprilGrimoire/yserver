# Animated Cursors (RENDER CreateAnimCursor) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Real frame cycling for RENDER `CreateAnimCursor` (opcode 31) on the KMS v2 backend, honoring per-frame delays; ynest keeps static frame 0.

**Architecture:** Backend-internal animation per `docs/superpowers/specs/2026-06-10-animated-cursors-design.md`. Core gains an `anim` flag on the Cursor resource + a `create_anim_cursor` trait method (default `None` → static fallback). KMS snapshots frames at creation, arms a deadline reported via `next_wakeup()`, and ticks at the top of `maybe_composite()` after its DPMS/VT gates. Per-tick minted versions keep the XFixes serial monotonic.

**Tech Stack:** Rust, existing KMS v2 backend (`crates/yserver/src/kms/v2/`), core dispatch (`crates/yserver-core/src/core_loop/process_request.rs`).

**Conventions (from AGENTS.md):** `cargo +nightly fmt`, plain `cargo clippy` (NOT pedantic), feature branch `feat/anim-cursor` (already created), squash merge at the end with user confirmation.

---

### Task 1: Core — `anim` flag on the Cursor resource

**Files:**
- Modify: `crates/yserver-core/src/resources.rs:2798-2808` (struct), `:2141-2163` (constructors), after `:2203` (new accessors)
- Test: inline `mod tests` at `crates/yserver-core/src/resources.rs:2810`

- [ ] **Step 1: Write the failing test**

Add to the `mod tests` block in `resources.rs` (after the existing tests):

```rust
#[test]
fn cursor_anim_flag_default_false_settable_and_freed() {
    let mut table = ResourceTable::new();
    let id = ResourceId(0x500);
    table.create_cursor(ClientId(1), id);
    assert!(!table.cursor_is_anim(id), "fresh cursor must not be anim");
    table.set_cursor_anim(id);
    assert!(table.cursor_is_anim(id));
    // Unknown id is never anim.
    assert!(!table.cursor_is_anim(ResourceId(0x501)));
    // create_glyph_cursor also defaults to false.
    let gid = ResourceId(0x502);
    table.create_glyph_cursor(ClientId(1), gid);
    assert!(!table.cursor_is_anim(gid));
}
```

Note: check how existing tests in that module construct `ResourceTable` (look at the first test in the module); if it's not `ResourceTable::new()`, copy the existing construction idiom.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p yserver-core cursor_anim_flag -- --nocapture`
Expected: COMPILE ERROR — `cursor_is_anim`/`set_cursor_anim` not found (that's the failure mode for missing API).

- [ ] **Step 3: Implement**

In `resources.rs:2798` add the field to `Cursor` (after `name_atom`):

```rust
    /// True iff this cursor was created by RENDER `CreateAnimCursor`.
    /// Consulted to reject nested animated cursors with `BadMatch`
    /// (Xorg `render/animcur.c:316` refuses them). Set on EVERY
    /// successful CreateAnimCursor — including backends that
    /// degenerate to the static first frame.
    pub anim: bool,
```

Add `anim: false,` to the two struct literals in `create_glyph_cursor` (`:2141`) and `create_cursor` (`:2153`). Then grep for any OTHER `Cursor {` literal construction sites and add `anim: false` there too:

Run: `grep -rn "Cursor {" crates/yserver-core/src/ crates/yserver/src/ | grep -v "CursorRecord\|CursorHandle\|CursorEntry\|AnimCursor\|ActiveCursor"`

Add the accessors after `free_cursor` (`:2199-2203`):

```rust
    /// Mark a cursor as animated (RENDER CreateAnimCursor product).
    pub fn set_cursor_anim(&mut self, id: ResourceId) {
        if let Some(c) = self.cursors.get_mut(&id.0) {
            c.anim = true;
        }
    }

    /// True iff `id` is a live animated cursor. Unknown ids are false.
    #[must_use]
    pub fn cursor_is_anim(&self, id: ResourceId) -> bool {
        self.cursors.get(&id.0).is_some_and(|c| c.anim)
    }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p yserver-core cursor_anim_flag`
Expected: PASS. Also run `cargo build --locked` to confirm no other `Cursor {` literal broke.

- [ ] **Step 5: Commit**

```bash
cargo +nightly fmt && git add -A && git commit -m "feat(core): anim flag on Cursor resource for nested-anim BadMatch"
```

---

### Task 2: Backend trait — `create_anim_cursor` with static-fallback default

**Files:**
- Modify: `crates/yserver-core/src/backend/trait_def.rs` (after `free_cursor`, `:946`)

- [ ] **Step 1: Add the trait method**

Insert after the `free_cursor` default method (`trait_def.rs:944-946`):

```rust
    /// RENDER `CreateAnimCursor` (opcode 31). `frames` pairs each
    /// sub-cursor's host handle with its delay in milliseconds.
    /// Returns the new animated cursor's handle, or `Ok(None)` when
    /// the backend does not animate — the caller then falls back to
    /// the static degeneration (cursor aliases frame 0's handle).
    /// Spec: `docs/superpowers/specs/2026-06-10-animated-cursors-design.md`.
    fn create_anim_cursor(
        &mut self,
        _origin: Option<OriginContext>,
        _frames: &[(CursorHandle, u32)],
    ) -> io::Result<Option<CursorHandle>> {
        Ok(None)
    }
```

- [ ] **Step 2: Verify it compiles everywhere**

Run: `cargo build --locked`
Expected: clean build — default impl means ynest (`host_x11/trait_impl.rs`) and `RecordingBackend` (`backend/recording.rs`) need no changes.

- [ ] **Step 3: Commit**

```bash
cargo +nightly fmt && git add -A && git commit -m "feat(backend): create_anim_cursor trait method, default static fallback"
```

---

### Task 3: Handler — error fidelity + trait dispatch (opcode 31)

**Files:**
- Modify: `crates/yserver-core/src/core_loop/process_request.rs:1880-1960`
- Test: inline `mod tests` at `process_request.rs:22798` (pattern: `poly_fill_rectangle_unknown_drawable_returns_bad_drawable` at `:22980` — `ServerState::new()` + `install_client` + `RecordingBackend::new()` + `process_request(...)` + `read_all_available`; error reply: `buf[1]` = error code, `buf[10]` = major opcode)

RENDER's major opcode is **133**; the minor goes in `RequestHeader.data`.

- [ ] **Step 1: Write the failing tests**

Add to the tests module:

```rust
fn anim_cursor_body(cid: u32, elts: &[(u32, u32)]) -> Vec<u8> {
    let mut b = cid.to_le_bytes().to_vec();
    for (cur, delay) in elts {
        b.extend_from_slice(&cur.to_le_bytes());
        b.extend_from_slice(&delay.to_le_bytes());
    }
    b
}

fn seed_cursor(state: &mut ServerState, raw: u32, host: u32) {
    state.resources.create_cursor(ClientId(1), ResourceId(raw));
    state.resources.set_cursor_host_xid(
        ResourceId(raw),
        crate::backend::CursorHandle::from_raw(host).unwrap(),
    );
}

fn send_anim_cursor(
    state: &mut ServerState,
    backend: &mut RecordingBackend,
    seq: u16,
    body: &[u8],
) {
    process_request(
        state,
        backend,
        ClientId(1),
        SequenceNumber(seq),
        RequestHeader {
            opcode: 133,
            data: 31,
            length_units: u32::try_from(1 + body.len().div_ceil(4)).unwrap(),
        },
        body,
        None,
    )
    .expect("process_request");
}

#[test]
fn create_anim_cursor_empty_list_returns_bad_value() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let body = anim_cursor_body(0x4000, &[]);
    send_anim_cursor(&mut state, &mut backend, 1, &body);
    let bytes = read_all_available(&mut peer);
    assert!(bytes.len() >= 32, "expected error reply, got {bytes:02x?}");
    assert_eq!(bytes[1], x11::error::BAD_VALUE);
    assert_eq!(bytes[10], 133);
    assert!(!state.resources.cursor_exists(ResourceId(0x4000)));
}

#[test]
fn create_anim_cursor_odd_pairs_returns_bad_length() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    seed_cursor(&mut state, 0x3000, 0x77);
    // One full pair + 4 trailing bytes = not a multiple of 8.
    let mut body = anim_cursor_body(0x4000, &[(0x3000, 50)]);
    body.extend_from_slice(&0x3000u32.to_le_bytes());
    send_anim_cursor(&mut state, &mut backend, 1, &body);
    let bytes = read_all_available(&mut peer);
    assert!(bytes.len() >= 32);
    assert_eq!(bytes[1], x11::error::BAD_LENGTH);
    assert!(!state.resources.cursor_exists(ResourceId(0x4000)));
}

#[test]
fn create_anim_cursor_nested_anim_returns_bad_match() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    seed_cursor(&mut state, 0x3000, 0x77);
    // First anim cursor (fallback path on RecordingBackend).
    let body = anim_cursor_body(0x4000, &[(0x3000, 50)]);
    send_anim_cursor(&mut state, &mut backend, 1, &body);
    let _ = read_all_available(&mut peer); // no error expected
    assert!(state.resources.cursor_is_anim(ResourceId(0x4000)));
    // Second anim cursor referencing the first → BadMatch.
    let body2 = anim_cursor_body(0x4001, &[(0x4000, 50)]);
    send_anim_cursor(&mut state, &mut backend, 2, &body2);
    let bytes = read_all_available(&mut peer);
    assert!(bytes.len() >= 32);
    assert_eq!(bytes[1], x11::error::BAD_MATCH);
    assert!(!state.resources.cursor_exists(ResourceId(0x4001)));
}

#[test]
fn create_anim_cursor_fallback_aliases_first_frame_and_sets_anim() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    seed_cursor(&mut state, 0x3000, 0x77);
    seed_cursor(&mut state, 0x3001, 0x78);
    let body = anim_cursor_body(0x4000, &[(0x3000, 50), (0x3001, 75)]);
    send_anim_cursor(&mut state, &mut backend, 1, &body);
    let bytes = read_all_available(&mut peer);
    assert!(bytes.is_empty(), "no error expected, got {bytes:02x?}");
    assert_eq!(state.resources.cursor_host_xid(ResourceId(0x4000)), Some(0x77));
    assert!(state.resources.cursor_is_anim(ResourceId(0x4000)));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p yserver-core create_anim_cursor -- --nocapture`
Expected: `empty_list` and `odd_pairs` FAIL (current code returns silent `Handled`); `nested_anim` FAILS (no BadMatch); `fallback_aliases` FAILS on the `cursor_is_anim` assert.

- [ ] **Step 3: Rewrite the opcode-31 arm**

Replace the body of the `31 => { ... }` arm at `process_request.rs:1880-1960`. Keep the existing leading comment block but update it: animation is now delegated to `backend.create_anim_cursor`; backends returning `None` degenerate to frame 0.

**Change the length guard** at `:1899` from `if body.len() < 12` to `if body.len() < 4` — the old `< 12` guard silently swallowed a cid-only body (4 bytes, zero frames), which must now reach the `BadValue` check below. `< 4` keeps the silent-return only for a truly unparseable body (no cid).

Keep the `cursor_id` parse and `validation_failed` check (`:1902-1921`) exactly as they are, then replace from `let pairs = ...` (line 1922) to the end of the arm with:

```rust
            let pairs = &body[4..];
            // Xorg fidelity (render.c:1796,1801): odd request length
            // → BadLength; zero frames → BadValue.
            if !pairs.len().is_multiple_of(8) {
                return emit_x11_error(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_LENGTH,
                    0,
                    header.opcode,
                );
            }
            if pairs.is_empty() {
                return emit_x11_error(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_VALUE,
                    0,
                    header.opcode,
                );
            }
            let mut first_host: Option<u32> = None;
            let mut frames: Vec<(crate::backend::CursorHandle, u32)> =
                Vec::with_capacity(pairs.len() / 8);
            for chunk in pairs.chunks_exact(8) {
                let sub = ResourceId(u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
                let delay = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]);
                if !state.resources.cursor_exists(sub) {
                    return emit_x11_error(
                        state,
                        client_id,
                        sequence,
                        x11::error::BAD_CURSOR,
                        sub.0,
                        header.opcode,
                    );
                }
                // Xorg refuses nested animated cursors (animcur.c:316).
                if state.resources.cursor_is_anim(sub) {
                    return emit_x11_error(
                        state,
                        client_id,
                        sequence,
                        x11::error::BAD_MATCH,
                        sub.0,
                        header.opcode,
                    );
                }
                if let Some(host_raw) = state.resources.cursor_host_xid(sub) {
                    if first_host.is_none() {
                        first_host = Some(host_raw);
                    }
                    if let Some(h) = crate::backend::CursorHandle::from_raw(host_raw) {
                        frames.push((h, delay));
                    }
                }
            }
            // Backend-side animation. `Ok(None)` (default impl /
            // ynest) → static degeneration to frame 0's handle. A
            // backend Err is "can't happen" after the validation
            // above — log and degenerate rather than swallowing
            // silently (spec "Error handling").
            let anim_handle = if frames.is_empty() {
                None
            } else {
                match backend.create_anim_cursor(origin, &frames) {
                    Ok(handle) => handle,
                    Err(e) => {
                        log::warn!(
                            "client {} RENDER::CreateAnimCursor backend failure ({e}); \
                             degenerating to first frame",
                            client_id.0,
                        );
                        None
                    }
                }
            };
            state.resources.create_glyph_cursor(client_id, cursor_id);
            state.resources.set_cursor_anim(cursor_id);
            if let Some(handle) = anim_handle {
                state.resources.set_cursor_host_xid(cursor_id, handle);
                log::debug!(
                    "client {} RENDER::CreateAnimCursor cursor=0x{:x} animated \
                     ({} frames, backend handle 0x{:x})",
                    client_id.0,
                    cursor_id.0,
                    frames.len(),
                    handle.as_raw(),
                );
            } else if let Some(host_raw) = first_host
                && let Some(handle) = crate::backend::CursorHandle::from_raw(host_raw)
            {
                state.resources.set_cursor_host_xid(cursor_id, handle);
                log::debug!(
                    "client {} RENDER::CreateAnimCursor cursor=0x{:x} (static \
                     degeneration to first sub-cursor host_xid=0x{host_raw:x})",
                    client_id.0,
                    cursor_id.0,
                );
            }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p yserver-core create_anim_cursor`
Expected: all 4 PASS.

- [ ] **Step 5: Commit**

```bash
cargo +nightly fmt && git add -A && git commit -m "feat(render): CreateAnimCursor error fidelity + backend dispatch"
```

---

### Task 4: KMS — `AnimCursorRecord` + `create_anim_cursor` impl

**Files:**
- Modify: `crates/yserver/src/kms/v2/cursor.rs` (new structs after `CursorRecord` impl, `:91`)
- Modify: `crates/yserver/src/kms/v2/backend.rs` (struct fields near `:290`; constructors near `:699`, `:824`, `:1486`; trait impl after `create_glyph_cursor`)
- Test: `mod tests` in `backend.rs` (pattern: `cursor_record_versions_monotonic` at `:16611`)

- [ ] **Step 1: Write the failing test**

Add to the backend tests module (next to `cursor_record_versions_monotonic`):

```rust
/// CreateAnimCursor snapshots frames at creation: maps gain an
/// entry aliasing frame 0; the AnimCursorRecord holds Arc'd frame
/// records + clamped delays.
#[test]
fn create_anim_cursor_snapshots_frames() {
    use std::time::Duration;
    use yserver_core::backend::{Backend, CursorHandle, PixmapHandle};

    let mut b = KmsBackendV2::for_tests();
    let pix = PixmapHandle::from_raw(0x1234_0030).unwrap();
    let c1 = b
        .create_cursor(None, pix, None, (0xFFFF, 0, 0), (0, 0, 0), 1, 2)
        .expect("c1");
    let c2 = b
        .create_cursor(None, pix, None, (0, 0xFFFF, 0), (0, 0, 0), 3, 4)
        .expect("c2");

    let anim = b
        .create_anim_cursor(None, &[(c1, 50), (c2, 0)])
        .expect("create_anim_cursor")
        .expect("KMS animates");

    let rec = b
        .anim_cursor_records
        .get(&anim.as_raw())
        .expect("anim record");
    assert_eq!(rec.frames.len(), 2);
    assert_eq!(rec.frames[0].delay, Duration::from_millis(50));
    // Delay 0 clamps to 16ms (spec: explicit Xorg deviation).
    assert_eq!(rec.frames[1].delay, Duration::from_millis(16));
    // The anim handle aliases frame 0 in the canonical map.
    assert_eq!(
        b.cursor_records.get(&anim.as_raw()).unwrap().version,
        b.cursor_records.get(&c1.as_raw()).unwrap().version,
    );
    // Unknown sub-cursor handle → error, no partial state.
    let bogus = CursorHandle::from_raw(0xDEAD_BEEF).unwrap();
    let before = b.anim_cursor_records.len();
    assert!(b.create_anim_cursor(None, &[(c1, 10), (bogus, 10)]).is_err());
    assert_eq!(b.anim_cursor_records.len(), before);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p yserver create_anim_cursor_snapshots -- --nocapture`
Expected: COMPILE ERROR — `anim_cursor_records` / `AnimCursorRecord` don't exist.

- [ ] **Step 3: Implement the data model**

In `cursor.rs` after the `CursorRecord` impl (`:91`):

```rust
/// One frame of a RENDER animated cursor (spec
/// `2026-06-10-animated-cursors-design.md`). Snapshotted at
/// `create_anim_cursor` time so constituent-cursor lifetime is a
/// non-issue (Xorg refcounts; we snapshot).
pub(crate) struct AnimFrame {
    pub(crate) record: Arc<CursorRecord>,
    /// Sprite pixmap the SW scene path samples. `None` when the
    /// sub-cursor's sprite alloc was skipped (Vk-less test
    /// fixtures) — mirrors `insert_cursor_record`'s best-effort
    /// `cursor_pixmaps` insert.
    pub(crate) pixmap: Option<crate::kms::v2::store::DrawableId>,
    pub(crate) delay: std::time::Duration,
}

/// Frame list for one animated cursor, keyed by the anim cursor's
/// host handle in `KmsBackendV2::anim_cursor_records`.
pub(crate) struct AnimCursorRecord {
    pub(crate) frames: Vec<AnimFrame>,
}

/// Live animation state — at most one, mirroring the single
/// effective cursor. Armed/cleared by `sync_cursor_animation`,
/// advanced by `tick_cursor_animation`.
pub(crate) struct ActiveCursorAnim {
    /// Animated cursor (host handle) whose frames are cycling.
    pub(crate) handle: u32,
    /// Current frame index into `AnimCursorRecord::frames`.
    pub(crate) frame: usize,
    /// Deadline for the next advance. Reported via `next_wakeup()`
    /// while outputs are active.
    pub(crate) next_frame: std::time::Instant,
}
```

In `backend.rs` add fields after `effective_cursor_xid` (`:290`):

```rust
    /// Animated-cursor frame lists, keyed by the anim cursor's host
    /// handle. Same key-space discipline as `cursor_records` /
    /// `cursor_pixmaps` (see comment at `cursor_records`); entries
    /// are never removed (status-quo no-op `free_cursor` — spec
    /// "Frame lifetime").
    pub(crate) anim_cursor_records:
        HashMap<u32, crate::kms::v2::cursor::AnimCursorRecord>,
    /// The one running animation (the effective cursor is animated),
    /// or `None`.
    pub(crate) active_cursor_anim: Option<crate::kms::v2::cursor::ActiveCursorAnim>,
```

Add `anim_cursor_records: HashMap::new(), active_cursor_anim: None,` to all three constructors — directly after each `cursor_pixmaps: HashMap::new(),` line (`:700`, `:825`, `:1486`).

Implement the trait method in the `impl Backend for KmsBackendV2` block, after `create_glyph_cursor`:

```rust
    fn create_anim_cursor(
        &mut self,
        _origin: Option<OriginContext>,
        frames: &[(CursorHandle, u32)],
    ) -> io::Result<Option<CursorHandle>> {
        // Spec 2026-06-10-animated-cursors-design.md. Snapshot every
        // frame up front — no partial map state on failure.
        if frames.is_empty() {
            return Ok(None);
        }
        let mut snap = Vec::with_capacity(frames.len());
        for (h, delay_ms) in frames {
            let raw = h.as_raw();
            let Some(record) = self.cursor_records.get(&raw) else {
                return Err(io::Error::other(format!(
                    "create_anim_cursor: unknown sub-cursor handle 0x{raw:x}"
                )));
            };
            // Delay 0 → 16ms: a 0 deadline would busy-spin the
            // poll loop (explicit Xorg deviation, see spec).
            let ms = if *delay_ms == 0 { 16 } else { *delay_ms };
            snap.push(crate::kms::v2::cursor::AnimFrame {
                record: std::sync::Arc::clone(record),
                pixmap: self.cursor_pixmaps.get(&raw).copied(),
                delay: std::time::Duration::from_millis(u64::from(ms)),
            });
        }
        let xid = self.core.next_host_xid();
        let handle = CursorHandle::from_raw(xid)
            .ok_or_else(|| io::Error::other("create_anim_cursor: xid was 0"))?;
        // Alias frame 0 in the canonical maps so every static-cursor
        // code path (effective walk, XFixes, scene) works untouched.
        self.cursor_records
            .insert(xid, std::sync::Arc::clone(&snap[0].record));
        if let Some(p) = snap[0].pixmap {
            self.cursor_pixmaps.insert(xid, p);
        }
        self.anim_cursor_records
            .insert(xid, crate::kms::v2::cursor::AnimCursorRecord { frames: snap });
        Ok(Some(handle))
    }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p yserver create_anim_cursor_snapshots`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo +nightly fmt && git add -A && git commit -m "feat(kms): AnimCursorRecord snapshot + create_anim_cursor impl"
```

---

### Task 5: KMS — arm/clear animation on effective-cursor change

**Files:**
- Modify: `crates/yserver/src/kms/v2/backend.rs:1122-1197` (`refresh_effective_cursor` split)
- Test: `mod tests` in `backend.rs`

- [ ] **Step 1: Write the failing test**

```rust
/// Effective-cursor change arms/clears the animation; re-resolving
/// to the same cursor preserves the running frame index.
#[test]
fn effective_cursor_arms_and_clears_animation() {
    use yserver_core::backend::{Backend, PixmapHandle};

    let mut b = KmsBackendV2::for_tests();
    let pix = PixmapHandle::from_raw(0x1234_0040).unwrap();
    let c1 = b
        .create_cursor(None, pix, None, (0xFFFF, 0, 0), (0, 0, 0), 0, 0)
        .expect("c1");
    let c2 = b
        .create_cursor(None, pix, None, (0, 0xFFFF, 0), (0, 0, 0), 0, 0)
        .expect("c2");
    let anim = b
        .create_anim_cursor(None, &[(c1, 50), (c2, 75)])
        .expect("anim")
        .expect("KMS animates");

    // Bind the anim cursor on root → it becomes effective and arms.
    let root_host = b.core.window_id;
    b.define_cursor(None, root_host, anim.as_raw()).expect("define anim");
    let st = b.active_cursor_anim.as_ref().expect("armed");
    assert_eq!(st.handle, anim.as_raw());
    assert_eq!(st.frame, 0);
    // Arming mints a fresh version for frame 0 (monotonic serial).
    let v_frame0 = b.cursor_records.get(&c1.as_raw()).unwrap().version;
    let v_anim = b.cursor_records.get(&anim.as_raw()).unwrap().version;
    assert!(v_anim > v_frame0, "armed version must be minted, not aliased");

    // Pretend the animation advanced, then re-resolve to the SAME
    // cursor: frame index must be preserved (no restart).
    b.active_cursor_anim.as_mut().unwrap().frame = 1;
    b.refresh_effective_cursor();
    assert_eq!(b.active_cursor_anim.as_ref().unwrap().frame, 1);

    // Switch to a static cursor → animation cleared.
    b.define_cursor(None, root_host, c1.as_raw()).expect("define static");
    assert!(b.active_cursor_anim.is_none());

    // Switch back → restarts at frame 0.
    b.define_cursor(None, root_host, anim.as_raw()).expect("re-define anim");
    assert_eq!(b.active_cursor_anim.as_ref().unwrap().frame, 0);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p yserver effective_cursor_arms -- --nocapture`
Expected: COMPILE ERROR (`active_cursor_anim` exists from Task 4, but nothing arms it → first `expect("armed")` panics after it compiles; either failure mode is fine).

- [ ] **Step 3: Implement**

Split `refresh_effective_cursor` (`backend.rs:1122-1197`). The function keeps its resolution head and delegates the display tail; `sync_cursor_animation` runs between them so the minted frame-0 record is what gets displayed:

```rust
    fn refresh_effective_cursor(&mut self) {
        let pointer_window = self.core.prev_pointer_window.unwrap_or(self.core.window_id);
        let new_xid = self.effective_cursor_walking_chain(pointer_window);
        if new_xid == self.effective_cursor_xid {
            // Same effective cursor — a running animation keeps its
            // frame index (Xorg: "already current → do nothing").
            return;
        }
        self.effective_cursor_xid = new_xid;
        self.sync_cursor_animation(new_xid);
        let Some(xid) = new_xid else {
            return;
        };
        self.display_cursor_by_handle(xid);
    }

    /// Arm (reset to frame 0) or clear the cursor animation for the
    /// new effective cursor. Arming swaps the canonical maps to
    /// frame 0 under a freshly-minted version so the XFixes serial
    /// stays monotonic (spec "Version/serial").
    fn sync_cursor_animation(&mut self, new_xid: Option<u32>) {
        let Some(xid) = new_xid else {
            self.active_cursor_anim = None;
            return;
        };
        let Some(anim) = self.anim_cursor_records.get(&xid) else {
            self.active_cursor_anim = None;
            return;
        };
        let first = &anim.frames[0];
        let (record, pixmap, delay) =
            (std::sync::Arc::clone(&first.record), first.pixmap, first.delay);
        self.swap_anim_frame_into_maps(xid, &record, pixmap);
        self.active_cursor_anim = Some(crate::kms::v2::cursor::ActiveCursorAnim {
            handle: xid,
            frame: 0,
            next_frame: std::time::Instant::now() + delay,
        });
    }

    /// Re-point the canonical maps at an animation frame under a
    /// freshly-minted monotonic version. The byte clone is bounded
    /// by cursor size (≤16 KiB for HW-plane cursors).
    fn swap_anim_frame_into_maps(
        &mut self,
        xid: u32,
        record: &std::sync::Arc<crate::kms::v2::cursor::CursorRecord>,
        pixmap: Option<crate::kms::v2::store::DrawableId>,
    ) {
        let version = self.next_cursor_version;
        self.next_cursor_version = self.next_cursor_version.saturating_add(1);
        let minted = crate::kms::v2::cursor::CursorRecord::new(
            record.width,
            record.height,
            record.hot_x,
            record.hot_y,
            record.bgra_bytes.clone(),
            version,
        );
        self.cursor_records.insert(xid, minted);
        // Keep cursor_pixmaps truthful per-frame: a `None` frame
        // REMOVES the entry — leaving the prior frame's pixmap
        // installed would have the SW scene path sample stale bytes.
        // (HW upload is unaffected; it consumes record bytes.)
        match pixmap {
            Some(p) => {
                self.cursor_pixmaps.insert(xid, p);
            }
            None => {
                self.cursor_pixmaps.remove(&xid);
            }
        }
    }
```

`display_cursor_by_handle` is the existing tail of `refresh_effective_cursor` (`:1129-1196`) moved verbatim into its own method — INCLUDING the sample-view readiness guard (`:1138-1148`):

```rust
    /// Push `cursor_records[xid]` to the scene / HW plane — the
    /// former tail of `refresh_effective_cursor`, shared with the
    /// animation tick. Keeps the sample-view readiness guard
    /// (Vk-less fixtures build records without sprite allocs).
    fn display_cursor_by_handle(&mut self, xid: u32) {
        let Some(record) = self.cursor_records.get(&xid).cloned() else {
            return;
        };
        let Some(&pixmap_id) = self.cursor_pixmaps.get(&xid) else {
            return;
        };
        // ... existing body from backend.rs:1138-1196 unchanged
        // (readiness guard, scene.register_cursor, Hw/Mixed
        // queue_steady_state_cursor_upload) ...
    }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p yserver effective_cursor`
Expected: the new test AND the pre-existing `effective_cursor_walks_parent_chain` + `define_cursor_records_per_window_and_root_sticky` all PASS (the refactor must not change static-cursor behavior).

- [ ] **Step 5: Commit**

```bash
cargo +nightly fmt && git add -A && git commit -m "feat(kms): arm/clear cursor animation on effective-cursor change"
```

---

### Task 6: KMS — frame tick + `next_wakeup` deadline + gating

**Files:**
- Modify: `crates/yserver/src/kms/v2/backend.rs:8217-8240` (`next_wakeup`), `:8264-8266` (`maybe_composite` tick call), new `tick_cursor_animation` near `sync_cursor_animation`
- Test: `mod tests` in `backend.rs`

- [ ] **Step 1: Write the failing tests**

```rust
/// Frame tick: advances mod n, re-arms relative, mints strictly
/// increasing versions across a full wraparound (XFixes serial
/// contract — naive Arc-swapping would repeat v1,v2,v1).
#[test]
fn anim_tick_advances_wraps_and_stays_monotonic() {
    use std::time::{Duration, Instant};
    use yserver_core::backend::{Backend, PixmapHandle};

    let mut b = KmsBackendV2::for_tests();
    let pix = PixmapHandle::from_raw(0x1234_0050).unwrap();
    let c1 = b
        .create_cursor(None, pix, None, (0xFFFF, 0, 0), (0, 0, 0), 0, 0)
        .expect("c1");
    let c2 = b
        .create_cursor(None, pix, None, (0, 0xFFFF, 0), (0, 0, 0), 0, 0)
        .expect("c2");
    let anim = b
        .create_anim_cursor(None, &[(c1, 50), (c2, 75)])
        .expect("anim")
        .expect("KMS animates");
    let root_host = b.core.window_id;
    b.define_cursor(None, root_host, anim.as_raw()).expect("define");

    let mut last_version = b.cursor_records.get(&anim.as_raw()).unwrap().version;
    let mut expected_frame = 0usize;
    // 5 ticks over 2 frames = two full wraparounds.
    for i in 0..5 {
        // Force the deadline into the past, then tick.
        b.active_cursor_anim.as_mut().unwrap().next_frame =
            Instant::now() - Duration::from_millis(1);
        b.tick_cursor_animation();
        expected_frame = (expected_frame + 1) % 2;
        let st = b.active_cursor_anim.as_ref().expect("still armed");
        assert_eq!(st.frame, expected_frame, "tick {i}");
        assert!(st.next_frame > Instant::now() - Duration::from_millis(1));
        let v = b.cursor_records.get(&anim.as_raw()).unwrap().version;
        assert!(v > last_version, "tick {i}: version {v} !> {last_version}");
        last_version = v;
        // The canonical record now carries the frame's bytes.
        let frame_rec = &b.anim_cursor_records.get(&anim.as_raw()).unwrap().frames
            [expected_frame]
            .record;
        assert_eq!(
            b.cursor_records.get(&anim.as_raw()).unwrap().bgra_bytes,
            frame_rec.bgra_bytes,
        );
    }
    // Tick before the deadline → no advance.
    let frame_before = b.active_cursor_anim.as_ref().unwrap().frame;
    b.tick_cursor_animation();
    assert_eq!(b.active_cursor_anim.as_ref().unwrap().frame, frame_before);
}

/// next_wakeup reports the anim deadline only while outputs are
/// active and scanout is allowed (EINVAL-storm discipline).
#[test]
fn anim_deadline_gated_on_outputs_active() {
    use std::time::{Duration, Instant};
    use yserver_core::backend::{Backend, PixmapHandle};

    let mut b = KmsBackendV2::for_tests();
    let pix = PixmapHandle::from_raw(0x1234_0060).unwrap();
    let c1 = b
        .create_cursor(None, pix, None, (0xFFFF, 0, 0), (0, 0, 0), 0, 0)
        .expect("c1");
    let anim = b
        .create_anim_cursor(None, &[(c1, 50)])
        .expect("anim")
        .expect("KMS animates");
    let root_host = b.core.window_id;
    b.define_cursor(None, root_host, anim.as_raw()).expect("define");
    let deadline = b.active_cursor_anim.as_ref().unwrap().next_frame;

    let wake = b.next_wakeup().expect("deadline reported");
    assert!(wake <= deadline);

    // DPMS off → deadline not reported, tick is a no-op.
    b.kms_outputs_active = false;
    let frame = b.active_cursor_anim.as_ref().unwrap().frame;
    b.active_cursor_anim.as_mut().unwrap().next_frame =
        Instant::now() - Duration::from_millis(1);
    assert!(
        b.next_wakeup().map_or(true, |w| w > Instant::now()),
        "stale anim deadline must not be reported while outputs are off",
    );
    b.tick_cursor_animation();
    assert_eq!(
        b.active_cursor_anim.as_ref().unwrap().frame,
        frame,
        "tick must not advance while outputs are off",
    );

    // Outputs back on with the deadline in the past → exactly one
    // immediate advance (spec: no fast-forward through missed frames).
    b.kms_outputs_active = true;
    b.tick_cursor_animation();
    assert_eq!(b.active_cursor_anim.as_ref().unwrap().frame, frame, "1-frame anim wraps to same index");
    assert!(
        b.active_cursor_anim.as_ref().unwrap().next_frame > Instant::now() - Duration::from_millis(5),
        "re-armed from now",
    );
}
```

Note for the gating test: `for_tests()` initializes `kms_outputs_active: true`. If `next_wakeup()` in the fixture returns an unrelated scene deadline, the `wake <= deadline` assert still holds (min-chaining); the DPMS-off assert tolerates other deadlines by checking only that no *stale* (past) instant is returned.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p yserver anim_tick anim_deadline -- --nocapture`
Expected: COMPILE ERROR — `tick_cursor_animation` doesn't exist.

- [ ] **Step 3: Implement**

Add next to `sync_cursor_animation`:

```rust
    /// Advance the running cursor animation if its deadline elapsed.
    /// Called from `maybe_composite` AFTER its scanout/DPMS gates
    /// (spec "Frame tick"). One advance per call — a stale deadline
    /// after a blank advances a single frame, never fast-forwards.
    pub(crate) fn tick_cursor_animation(&mut self) {
        if !self.kms_outputs_active || !self.scanout_allowed() {
            return;
        }
        let now = std::time::Instant::now();
        let Some(st) = self.active_cursor_anim.as_ref() else {
            return;
        };
        if now < st.next_frame {
            return;
        }
        let handle = st.handle;
        let current = st.frame;
        let Some(anim) = self.anim_cursor_records.get(&handle) else {
            self.active_cursor_anim = None;
            return;
        };
        let next = (current + 1) % anim.frames.len();
        let frame = &anim.frames[next];
        let (record, pixmap, delay) =
            (std::sync::Arc::clone(&frame.record), frame.pixmap, frame.delay);
        self.swap_anim_frame_into_maps(handle, &record, pixmap);
        if let Some(st) = self.active_cursor_anim.as_mut() {
            st.frame = next;
            st.next_frame = now + delay;
        }
        self.display_cursor_by_handle(handle);
    }

    /// Deadline for `next_wakeup`: the animation's next frame, only
    /// while it could actually be displayed (same gates as the tick).
    fn cursor_anim_deadline(&self) -> Option<std::time::Instant> {
        if !self.kms_outputs_active || !self.scanout_allowed() {
            return None;
        }
        self.active_cursor_anim.as_ref().map(|st| st.next_frame)
    }
```

In `next_wakeup` (`:8217`), chain the new deadline into the final `min`:

```rust
        scene_deadline
            .into_iter()
            .chain(present_deadline)
            .chain(self.cursor_anim_deadline())
            .min()
```

In `maybe_composite`, directly after the `kms_outputs_active` gate (`:8264-8266`), add:

```rust
        // Animated-cursor frame advance — after both gates above so
        // DPMS-off / VT-away never uploads (spec "DPMS / VT gating").
        self.tick_cursor_animation();
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p yserver anim_ && cargo test -p yserver cursor`
Expected: new tests PASS; all pre-existing cursor tests still PASS (including the `next_wakeup` tests at `backend.rs:14913,14928` — if they assert `next_wakeup().is_none()` on a fixture with no animation, they're unaffected).

- [ ] **Step 5: Commit**

```bash
cargo +nightly fmt && git add -A && git commit -m "feat(kms): animated cursor frame tick via next_wakeup + maybe_composite"
```

---

### Task 7: XFixes GetCursorImage returns the current frame

**Files:**
- Test only: `mod tests` in `backend.rs` (`get_active_cursor_image` at `:14024` already reads `cursor_records[effective]` — the tick's map swap makes it frame-correct; this task proves it)

- [ ] **Step 1: Write the test (expected to pass — regression lock)**

```rust
/// XFixes GetCursorImage tracks the animation: bytes follow the
/// current frame, serial strictly increases across a wraparound.
#[test]
fn xfixes_cursor_image_follows_animation_frames() {
    use std::time::{Duration, Instant};
    use yserver_core::backend::{Backend, PixmapHandle};

    let mut b = KmsBackendV2::for_tests();
    let pix = PixmapHandle::from_raw(0x1234_0070).unwrap();
    let c1 = b
        .create_cursor(None, pix, None, (0xFFFF, 0, 0), (0, 0, 0), 0, 0)
        .expect("c1");
    let c2 = b
        .create_cursor(None, pix, None, (0, 0xFFFF, 0), (0, 0, 0), 0, 0)
        .expect("c2");
    let anim = b
        .create_anim_cursor(None, &[(c1, 50), (c2, 75)])
        .expect("anim")
        .expect("KMS animates");
    let root_host = b.core.window_id;
    b.define_cursor(None, root_host, anim.as_raw()).expect("define");

    let mut last_serial = b
        .get_active_cursor_image()
        .expect("image")
        .serial;
    for _ in 0..4 {
        b.active_cursor_anim.as_mut().unwrap().next_frame =
            Instant::now() - Duration::from_millis(1);
        b.tick_cursor_animation();
        let img = b.get_active_cursor_image().expect("image");
        assert!(img.serial > last_serial, "serial must strictly increase");
        last_serial = img.serial;
        let frame_idx = b.active_cursor_anim.as_ref().unwrap().frame;
        let frame_rec = &b.anim_cursor_records.get(&anim.as_raw()).unwrap().frames
            [frame_idx]
            .record;
        assert_eq!(*img.bgra_bytes, frame_rec.bgra_bytes);
    }
}
```

- [ ] **Step 2: Run it**

Run: `cargo test -p yserver xfixes_cursor_image_follows`
Expected: PASS (the mechanism is Task 5/6's map swap). If it FAILS, the tick isn't updating `cursor_records` — fix there, not in `get_active_cursor_image`.

- [ ] **Step 3: Commit**

```bash
cargo +nightly fmt && git add -A && git commit -m "test(kms): XFixes cursor image follows animation frames"
```

---

### Task 8: Visual smoke probe + vng verification

**Files:**
- Create: `tools/anim-cursor-probe.c` (pattern: existing `tools/*-probe.c` clients)

- [ ] **Step 1: Write the probe client**

```c
/* anim-cursor-probe — visible smoke test for RENDER CreateAnimCursor.
 *
 * Creates a window whose cursor is a 2-frame animated cursor
 * (solid red / solid blue 24x24, 300ms per frame), then idles.
 * On a server with working animation the cursor visibly blinks
 * red/blue while hovering the window; on static-degeneration
 * servers it stays red.
 *
 * Build: cc -o target/anim-cursor-probe tools/anim-cursor-probe.c -lX11 -lXrender
 * Run:   DISPLAY=:7 ./target/anim-cursor-probe
 */
#include <stdio.h>
#include <stdlib.h>
#include <X11/Xlib.h>
#include <X11/extensions/Xrender.h>

static Cursor solid_cursor(Display *dpy, Window root,
                           unsigned short r, unsigned short g,
                           unsigned short b) {
    int w = 24, h = 24;
    Pixmap pm = XCreatePixmap(dpy, root, w, h, 32);
    XRenderPictFormat *fmt =
        XRenderFindStandardFormat(dpy, PictStandardARGB32);
    Picture pic = XRenderCreatePicture(dpy, pm, fmt, 0, NULL);
    XRenderColor c = { r, g, b, 0xFFFF };
    XRenderFillRectangle(dpy, PictOpSrc, pic, &c, 0, 0, w, h);
    Cursor cur = XRenderCreateCursor(dpy, pic, 0, 0);
    XRenderFreePicture(dpy, pic);
    XFreePixmap(dpy, pm);
    return cur;
}

int main(void) {
    Display *dpy = XOpenDisplay(NULL);
    if (!dpy) { fprintf(stderr, "cannot open display\n"); return 1; }
    Window root = DefaultRootWindow(dpy);

    XAnimCursor frames[2];
    frames[0].cursor = solid_cursor(dpy, root, 0xFFFF, 0, 0);
    frames[0].delay = 300;
    frames[1].cursor = solid_cursor(dpy, root, 0, 0, 0xFFFF);
    frames[1].delay = 300;
    Cursor anim = XRenderCreateAnimCursor(dpy, 2, frames);
    /* libXcursor pattern: constituents freed right after creation —
     * exercises the snapshot/keep-alive lifetime story. */
    XFreeCursor(dpy, frames[0].cursor);
    XFreeCursor(dpy, frames[1].cursor);

    Window win = XCreateSimpleWindow(dpy, root, 50, 50, 400, 300, 1,
                                     0x000000, 0xCCCCCC);
    XStoreName(dpy, win, "anim-cursor-probe");
    XDefineCursor(dpy, win, anim);
    XMapWindow(dpy, win);
    XSync(dpy, False);
    printf("anim cursor 0x%lx defined; hover the window — cursor "
           "should blink red/blue every 300ms. Ctrl-C to exit.\n",
           anim);
    for (;;) {
        XEvent ev;
        XNextEvent(dpy, &ev);
    }
}
```

- [ ] **Step 2: Build it**

Run: `cc -o target/anim-cursor-probe tools/anim-cursor-probe.c -lX11 -lXrender`
Expected: clean compile (libXrender headers are present — `tools/glx-tfp-probe.c` et al. already link X libs).

- [ ] **Step 3: ynest sanity (fallback path, no animation expected)**

Run yserver's ynest with `RUST_LOG=debug`, run the probe against it. Expected: probe runs without errors, cursor shows the **static red** frame (documented degeneration), log shows the `CreateAnimCursor` debug line. No BadCursor/BadMatch errors.

- [ ] **Step 4: vng smoke (the real check)**

Use the established vng harness (`tools/yserver-vng-run.sh` / memory `reference_virtme_ng_drm_harness`) to boot yserver on virtio-gpu KMS, run `anim-cursor-probe` inside, hover the window. Expected: cursor visibly blinks red/blue at ~300ms cadence. Also verify `left_ptr_watch` behavior with a real app launch if a DE is up.

This is a visible smoke check — per project convention it gates the interactive claim, and HW (bee) verification is coordinated with the user separately.

- [ ] **Step 5: Commit**

```bash
git add tools/anim-cursor-probe.c && git commit -m "tools: anim-cursor-probe visible smoke client"
```

---

### Task 9: Full validation + status doc

- [ ] **Step 1: Full test suite**

Run: `cargo test --locked`
Expected: all green.

- [ ] **Step 2: Lint + format**

Run: `cargo clippy` (plain — NOT pedantic, per AGENTS.md) and `cargo +nightly fmt --check`
Expected: zero warnings introduced by this branch.

- [ ] **Step 3: Update docs/status.md**

Add a line under the current status section noting RENDER CreateAnimCursor now animates on KMS v2 (was: static first frame), ynest unchanged.

- [ ] **Step 4: Commit**

```bash
git add -A && git commit -m "docs: status — animated cursors on KMS v2"
```

- [ ] **Step 5: Hand off**

Branch `feat/anim-cursor` ready: ask the user about HW dogfood on bee (busy cursor during app launch in MATE; DPMS off/on with spinner active → no EINVAL storm) and squash-merge confirmation (per AGENTS.md, ask first).

---

## Plan self-review notes

- **Spec coverage:** anim flag + errors (T1/T3), trait method (T2), snapshot data model + delay clamp + alias frame 0 (T4), arm/clear/preserve + minted frame-0 version (T5), tick + wakeup + gating + single-advance resume (T6), XFixes serial (T7), keep-alive lifetime exercised by the probe freeing constituents (T8), no `free_cursor` task — deliberate, spec "Frame lifetime" says status-quo no-op.
- **Type consistency:** `AnimFrame.pixmap: Option<DrawableId>` everywhere (spec amended 2026-06-10 after plan review — remove-on-None rule); `ActiveCursorAnim{handle, frame, next_frame}` used identically in T4-T7; `swap_anim_frame_into_maps` defined in T5, used in T6.
- **Known judgment calls:** `tick_cursor_animation` double-checks the gates internally (defense in depth — `maybe_composite` already gates; the internal check also serves the unit tests, which call the tick directly).
