# Grab-Ownership Unification + Xorg-Faithful Grab Semantics — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `active_pointer_grab` the single source of truth for the pointer grab, with an Xorg-faithful `AlreadyGrabbed` guard, implicit-grab owner selection, and grab lifetime — so the recurring "release routes to the wrong client / grab lingers" bug class collapses.

**Architecture:** Unify the three parallel grab fields into one record (foundation), migrate every reader/writer with a dual-write+assert safety phase, then land the three Xorg-faithful corrections (guard, attribution, lifetime) as one series pinned by tests, removing the non-Xorg `3312e24e` supersede hack LAST. Delete the legacy fields only after the dual-write assertions have held across the suite.

**Tech Stack:** Rust, `crates/yserver-core`. Reference implementation: `../xserver` (`dix/events.c`, `Xi/exevents.c`). Test oracles in repo root: `xfce-term.xtrace`/`xfce-term-xorg.xtrace`, `mate.xtrace`, `cinnamon.xtrace`.

---

## Spec

`docs/superpowers/specs/2026-07-16-grab-ownership-unification-design.md` (codex-approved). This plan **refines** the spec's §5 attribution mechanism after reading `../xserver` directly — see "Xorg ground truth" below. The refinement is consistent with the spec's *conclusion* (the app owns the implicit grab in the MATE case → SameClient → Success, no supersede) but corrects the cited mechanism and changes what two currently-passing tests assert.

### Scope boundary (codex review, finding 1): this is a LOCALIZED approximation, not "exactly Xorg"
Xorg's `DeliverDeviceEvents` walks **XI2 → XI1 → core** per window. This plan's attribution fix models only the **core-vs-XI2** subset, because yserver deliberately does **not** install implicit pointer grabs from plain XI1 `DeviceButtonPress` (`pointer_fanout.rs:1818`), so the XI1 level is moot for implicit-grab ownership. Do **not** describe the resolver as "exactly Xorg" — it is a faithful reproduction *for the core/XI2 case that the MATE/XFCE/Cinnamon bugs live in*. The XI1 omission is an explicit, documented boundary.

## Xorg ground truth (read from `../xserver` this session — the crux, §5)

The implicit pointer grab is created in `ActivateImplicitGrab` (`dix/events.c:2150`), called from `DeliverEventsToWindow` (`:2415-2422`) with the `(client, pWin, deliveryMask)` of the delivery. The driving walk is `DeliverDeviceEvents` (`:2885`):

```
while (pWin) {                       // pWin starts at the LEAF, walks toward root
    mask = EventIsDeliverable(dev, type, pWin);
    if (mask & EVENT_XI2_MASK) { deliver XI2; if delivered break; }   // XI2 FIRST
    if (mask & EVENT_XI1_MASK) { deliver XI;  if delivered break; }
    if (mask & EVENT_CORE_MASK && IsMaster) { deliver CORE; if delivered break; }  // core LAST
    if (deliveries<0 || pWin==stopAt || dontpropagate) break;
    pWin = pWin->parent;
}
```

**The rule:** the walk stops at the **deepest** window that has *any* selector for this device+event; within that window XI2 is tried before XI before core. The implicit grab is attributed to the client of *that* delivery, on *that* window, with the window's merged XI2 mask (`ActivateImplicitGrab` `:2183-2188 xi2mask_merge`).

**yserver today is inverted (root cause):** it captures `delivered_press` **core-first, globally** — `pointer_fanout.rs:1060` (core fanout, gated `delivered_press.is_none()`) runs before the XI2 fanout capture at `:1601` (also gated `is_none()`). So a core selector on an *ancestor* wins over an XI2 selector on the *leaf*. In the MATE combo (`mate.xtrace`: conn 013 core on parent, conn 046 XI2 on leaf), yserver attributes the implicit grab to conn 013; Xorg attributes it to conn 046 (leaf is deeper, XI2-first). That mis-attribution is exactly what the `3312e24e` supersede hack papers over.

**Localized fix (stays within the spec's "preserve delivery" scope, §7):** do NOT rewrite the fanout delivery sets. Only change which delivery is *credited* as the implicit-grab owner: pick the **deeper** of the core-delivery window and the XI2-delivery window, XI2 winning ties. yserver already computes both windows (`nested_id` for core, the XI2 target window in the XI2 fanout) and has `resources.is_descendant_of(candidate, ancestor)`.

## Current state (census, verified this session)

Fields on `ServerState` (`server.rs`):
- `pointer_grab: Option<(ClientId, ResourceId)>` (`:823`) — **DELETE**
- `pointer_grab_is_passive: bool` (`:837`) — **DELETE**
- `active_pointer_grab: Option<ActivePointerGrab>` (`:833`) — **KEEP** (sole source)

`ActivePointerGrab` (`server.rs:513`) already has `owner, grab_window, event_mask, cursor, time, owner_events, via_xi2, implicit, xi2_mask`. It lacks only `passive: bool`. The snapshot fields the spec asks for already exist — the real gap is that **passive activation never populates `active_pointer_grab` at all** (it writes only the two legacy fields).

Production write sites of the legacy fields that must be routed through the record (from census):
- `handle_grab_pointer` `process_request.rs:24066-24068` (already writes all three)
- XIGrabDevice `process_request.rs:11710-11712`; XIUngrabDevice `:11934-11936`
- `deactivate_core_pointer_grab` `:24204-24206`
- `deactivate_passive_pointer_grab_crossings` `:24184-24186` (writes legacy only — passive)
- `process_disconnect` `process_disconnect.rs:333-334, 351`
- `implicit_pointer_grab_lifecycle` `pointer_fanout.rs:1668-1670, 1694-1695`
- passive activation `pointer_fanout.rs:828-829` (**legacy only** — the desync source)
- `release_passive_grab_on_button_release` `pointer_fanout.rs:2599-2600` (legacy only — passive)
- legacy `server.rs` `pointer_event_fanout_inner` `:2650-2651, 2690-2691` (dead lock-based path)

Production reader sites that must migrate off the legacy fields:
- guard `handle_grab_pointer:24043`, XIGrabDevice `:11672/:11674`
- `active_grab_target:2423/:2433` (passive→`button_grabs` fallback)
- XI2 passive gate `pointer_fanout.rs:1224/:1229`
- `release_passive_grab_on_button_release:2597/:2598`, `deactivate_passive_pointer_grab_crossings:24184`
- `apply_allow_events:20034/:20145`, XIAllowEvents `:13915`
- `emit_barrier_event:2998`, `process_disconnect:330`, `release_core_grabs_for_unviewable:24756`, UngrabPointer `:24160`, XIUngrabDevice grab-window `:11903/:11931`
- legacy `server.rs` fanout `:2540/:2543` (dead path — migrate for consistency or leave last)

## File structure

All changes are in `crates/yserver-core/src/`:
- `server.rs` — struct field + `impl ServerState` grab helpers
- `core_loop/pointer_fanout.rs` — passive activation, attribution, lifetime, `active_grab_target`, XI2 gate
- `core_loop/process_request.rs` — guard, GrabPointer/Ungrab, XIGrabDevice/Ungrab, AllowEvents, deactivation helpers, tests
- `core_loop/process_disconnect.rs` — disconnect teardown

No new files. Tests live inline in the existing `#[cfg(test)] mod tests` of each file, next to the current grab tests.

## Ordering hazard (spec migration §4)

The `3312e24e` supersede exemption (`process_request.rs:11674-11675` `held_is_implicit`) is removed **LAST**, gated on the attribution (#5) and lifetime (#6) tests being green. Dropping it earlier re-breaks the MATE combo and the Cinnamon dialog. There must be no window where the pure guard runs without correct attribution+lifetime.

---

## Task 1: Add `passive` field + grab mutation helpers (foundation)

**Files:**
- Modify: `crates/yserver-core/src/server.rs:513-555` (struct), and add `impl ServerState` helpers
- Modify every `ActivePointerGrab { .. }` literal to add `passive:` (compile fix): `process_request.rs:11712, 24068`, `pointer_fanout.rs:1670`, and the test literals at `process_request.rs:48849`, `pointer_fanout.rs` (`implicit_grab_not_installed_under_explicit_grab` uses `ActivePointerGrab`), etc.

- [ ] **Step 1: Add the `passive` field**

In `ActivePointerGrab` (`server.rs`), after `implicit: bool` and before `xi2_mask`:

```rust
    /// True when this grab was activated by a passive button grab
    /// (Xorg `grabinfo->fromPassiveGrab`). Distinguishes an activated
    /// passive grab from an explicit GrabPointer/XIGrabDevice and from a
    /// press-driven implicit grab. Passive and implicit grabs share the
    /// auto-release lifetime; explicit grabs live until UngrabPointer.
    pub passive: bool,
```

- [ ] **Step 2: Add mutation helpers on `ServerState`**

Add to the `impl ServerState` block (near the existing grab-related methods; search `fn ` for an anchor such as the block containing pointer state):

```rust
    /// Single mutation path for the active pointer grab (spec: one source
    /// of truth). Every grab activation — explicit GrabPointer/XIGrabDevice,
    /// passive-grab activation, and press-driven implicit grab — builds a
    /// full `ActivePointerGrab` and installs it here.
    pub fn set_pointer_grab(&mut self, grab: ActivePointerGrab) {
        self.active_pointer_grab = Some(grab);
    }

    /// Single teardown path for the active pointer grab.
    pub fn clear_pointer_grab(&mut self) {
        self.active_pointer_grab = None;
    }
```

(These wrap the field for now; they become the *only* writers after Task 8. Keeping them thin lets the dual-write phase assert against the legacy fields.)

- [ ] **Step 3: Fix every `ActivePointerGrab { .. }` literal to include `passive:`**

Explicit grabs and implicit grabs set `passive: false`. Update each literal. Production:
- `process_request.rs:11712` (XIGrabDevice): add `passive: false,`
- `process_request.rs:24068` (GrabPointer): add `passive: false,`
- `pointer_fanout.rs:1670` (implicit): add `passive: false,`

Tests (add `passive: false,` unless the test models a passive grab):
- `process_request.rs:48849` (`xi_grab_device_supersedes_foreign_implicit_grab_mate_combo`) — `implicit: true` → `passive: false`
- any other `ActivePointerGrab { .. }` literal the compiler flags.

- [ ] **Step 4: Build**

Run: `cargo build -p yserver-core`
Expected: PASS (no missing-field errors).

- [ ] **Step 5: Commit**

```bash
git add crates/yserver-core/src/server.rs crates/yserver-core/src/core_loop/
git commit -m "feat(grab): add ActivePointerGrab.passive + set/clear_pointer_grab helpers"
```

---

## Task 2: Passive activation populates the full record (dual-write + assert)

This closes the desync at its source: passive activation currently sets only the legacy fields, leaving `active_pointer_grab = None`.

**Files:**
- Modify: `crates/yserver-core/src/core_loop/pointer_fanout.rs:817-830` (passive activation)
- Test: inline in `pointer_fanout.rs` tests

- [ ] **Step 1: Write the failing test**

Add to `pointer_fanout.rs` tests:

```rust
#[test]
fn passive_grab_activation_populates_active_pointer_grab_record() {
    // A passive button grab that matches on ButtonPress must populate the
    // single-source record (owner/window/passive/owner_events/via_xi2/mask),
    // not just the legacy pointer_grab tuple. Regression pin for the desync
    // that produced the Cinnamon AlreadyGrabbed bug.
    use crate::server::PassiveButtonGrab;
    const WM: u32 = 1;
    let mut state = ServerState::new();
    let _wm = install_client(&mut state, WM);
    let grab_win = ResourceId(0x0070_0003);
    state.resources.create_window(
        ClientId(WM),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24, window: grab_win, parent: ROOT_WINDOW, x: 0, y: 0,
            width: 100, height: 100, border_width: 0, class: 1,
            visual: crate::resources::ROOT_VISUAL, ..Default::default()
        },
    );
    let _ = state.resources.map_window(grab_win);
    state.button_grabs.push(PassiveButtonGrab {
        owner: ClientId(WM),
        grab_window: grab_win,
        button: 1,
        modifiers: 0x8000, // AnyModifier
        event_mask: (1 << 2) | (1 << 3), // ButtonPress|ButtonRelease
        owner_events: false,
        pointer_mode: 1, // async
        keyboard_mode: 1,
        confine_to: ResourceId(0),
        cursor: ResourceId(0),
        via_xi2: false,
        xi2_mask: 0,
    });
    let mut backend = crate::backend::testing::RecordingBackend::new();
    crate::backend::Backend::register_top_level(&mut backend, None, grab_win, 0xCAFE_0001)
        .expect("register");
    let xid_map = backend.xid_map().clone();
    let press = crate::host_x11::HostPointerEvent {
        kind: crate::host_x11::PointerEventKind::ButtonPress,
        host_xid: 0xCAFE_0001, detail: 1, time: 42,
        root_x: 10, root_y: 10, event_x: 10, event_y: 10,
        state: 0, crossing_mode: 0, child: 0, raw_dx: 0, raw_dy: 0,
    };
    let _ = pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, press, true, false);

    let g = state.active_pointer_grab.expect("passive activation must populate the record");
    assert_eq!(g.owner, ClientId(WM));
    assert_eq!(g.grab_window, grab_win);
    assert!(g.passive, "record must be flagged passive");
    assert!(!g.implicit);
    assert!(!g.via_xi2);
}
```

(Confirm `PassiveButtonGrab`'s exact field set against `server.rs` before running; match it verbatim.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p yserver-core passive_grab_activation_populates_active_pointer_grab_record`
Expected: FAIL — `active_pointer_grab` is `None` after passive activation.

- [ ] **Step 3: Populate the record in passive activation**

At `pointer_fanout.rs:827-829`, replace the legacy-only write with a dual write through the record + a consistency assertion:

```rust
        // Activate the passive grab atomically with the dispatch.
        state.pointer_grab = Some((grab.owner, grab.grab_window));
        state.pointer_grab_is_passive = true;
        state.set_pointer_grab(crate::server::ActivePointerGrab {
            owner: grab.owner,
            grab_window: grab.grab_window,
            event_mask: u16::try_from(grab.event_mask & 0xFFFF).unwrap_or(0),
            cursor: grab.cursor,
            time: event.time,
            owner_events: grab.owner_events,
            via_xi2: grab.via_xi2,
            implicit: false,
            passive: true,
            xi2_mask: grab.xi2_mask,
        });
        debug_assert_eq!(
            state.pointer_grab,
            state.active_pointer_grab.map(|g| (g.owner, g.grab_window)),
            "passive activation desync: legacy tuple vs record (owner, grab_window)",
        );
        debug_assert!(
            state.pointer_grab_is_passive
                == state.active_pointer_grab.is_some_and(|g| g.passive),
            "passive activation desync: legacy is_passive vs record.passive",
        );
```

(Check `PassiveButtonGrab`'s field names — `event_mask`, `xi2_mask`, `cursor` — and adjust the RHS to match. If `event_mask` is already `u16`, drop the `try_from`.)

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p yserver-core passive_grab_activation_populates_active_pointer_grab_record`
Expected: PASS.

- [ ] **Step 5: Run the whole grab suite (dual-write must not regress delivery)**

Run: `cargo test -p yserver-core grab`
Expected: PASS (all existing grab/allow-events tests still green — the record is now populated but no reader has switched to it yet).

- [ ] **Step 6: Commit**

```bash
git add crates/yserver-core/src/core_loop/pointer_fanout.rs
git commit -m "feat(grab): passive activation populates active_pointer_grab (dual-write)"
```

---

## Task 3: Route the remaining passive writers through the record (dual-write)

The other sites that write the legacy fields for a *passive* grab without touching the record: `release_passive_grab_on_button_release` (`pointer_fanout.rs:2599`) and `deactivate_passive_pointer_grab_crossings` (`process_request.rs:24184`). Both must also clear/keep the record consistent.

**Files:**
- Modify: `crates/yserver-core/src/core_loop/pointer_fanout.rs:2596-2600`
- Modify: `crates/yserver-core/src/core_loop/process_request.rs:24183-24192`

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn passive_grab_release_on_button_release_clears_the_record() {
    // After Task 2, passive activation populates the record; the release
    // path must clear it too, or the record lingers (foreign-grab desync).
    use crate::server::PassiveButtonGrab;
    // ... same window/grab setup as passive_grab_activation_populates_... ,
    // then a ButtonPress (activates) followed by a ButtonRelease.
    // Assert: after the release, state.active_pointer_grab.is_none().
}
```

(Reuse the setup helper pattern from Task 2's test; end with a `ButtonRelease` event fanout and assert `state.active_pointer_grab.is_none()`.)

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p yserver-core passive_grab_release_on_button_release_clears_the_record`
Expected: FAIL — record still `Some` after release.

- [ ] **Step 3: Clear the record in the passive-release path**

`pointer_fanout.rs:2596-2600`:

```rust
fn release_passive_grab_on_button_release(state: &mut ServerState, kind: PointerEventKind) {
    if kind == PointerEventKind::ButtonRelease && state.pointer_grab_is_passive {
        let grab = state.pointer_grab;
        state.pointer_grab = None;
        state.pointer_grab_is_passive = false;
        state.clear_pointer_grab();
```

`process_request.rs:24183-24186` (`deactivate_passive_pointer_grab_crossings`):

```rust
    let prev_grab_window = state.pointer_grab.map(|(_, w)| w);
    state.pointer_grab = None;
    state.pointer_grab_is_passive = false;
    state.clear_pointer_grab();
```

- [ ] **Step 4: Run to verify it passes + suite**

Run: `cargo test -p yserver-core passive_grab_release_on_button_release_clears_the_record && cargo test -p yserver-core grab`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/yserver-core/src/core_loop/
git commit -m "feat(grab): passive release paths clear the record (dual-write)"
```

---

## Task 4: Migrate `active_grab_target` off the passive fallback

`active_grab_target` (`pointer_fanout.rs:2412-2459`) reads `pointer_grab` for `(client, window)` and branches on `pointer_grab_is_passive` to re-scan `button_grabs` for flags. With the record now populated for passive grabs, read the record directly.

**Files:**
- Modify: `crates/yserver-core/src/core_loop/pointer_fanout.rs:2412-2459`

**Semantic-change warning (codex finding 5):** the current fallback rescans `button_grabs` by `(owner, grab_window)` with `.rev().find()` (`:2433-2438`) — it returns the **last-registered** grab on that key. The record, by contrast, snapshots the **actually-matched** grab from `try_match_passive_grab` (the one whose button+modifiers matched). When one client registers *two* passive grabs on the same window with different masks/modifiers, these differ. The record path is more correct (it's the grab that actually fired), but this IS a behavior change — the test below must distinguish the two so the change is intentional and pinned, not silent.

- [ ] **Step 1: Characterization test that DISTINGUISHES matched-vs-last-registered**

Register TWO passive grabs for the same owner+window with different masks/modifiers; make the press match the FIRST-registered (not the last); assert `active_grab_target` returns the MATCHED grab's mask. Under the current fallback this returns the last-registered grab's mask (so the test documents the pre-change behavior); after the swap it returns the matched grab's mask.

```rust
#[test]
fn active_grab_target_uses_the_matched_passive_grab_not_last_registered() {
    // Two passive grabs, same owner+window, different (modifiers, mask).
    // The press matches grab A (registered first). Xorg activates the
    // grab that MATCHED, so active_grab_target must carry A's mask —
    // even though B was registered later and the (owner,window) fallback
    // would pick B.
    // ... setup: push grab A {modifiers: <mods that match the press>,
    //     event_mask: MASK_A}, then grab B {modifiers: <non-matching>,
    //     event_mask: MASK_B}, same owner+grab_win ...
    // fanout a ButtonPress that matches A's modifiers.
    let (_win, _owner, _gx, _gy, _oe, _vx, mask) =
        active_grab_target(&state).expect("active grab present");
    assert_eq!(mask, MASK_A,
        "active_grab_target must reflect the grab that actually matched (A), \
         not the last-registered (B)");
}
```

Read `try_match_passive_grab` to construct modifiers that match A but not B. If the modifier model makes "matches A, not B" impossible to express, fall back to the simpler one-grab test but ADD an inline note that the matched-vs-last-registered divergence is unpinned and must be checked by hand.

- [ ] **Step 2: Run to verify it FAILS today** (fallback picks last-registered)

Run: `cargo test -p yserver-core active_grab_target_uses_the_matched_passive_grab_not_last_registered`
Expected: FAIL — the `(owner, grab_window)` fallback returns B's mask (last-registered), not A's (matched). This proves the divergence is real. (If you fell back to the one-grab test per Step 1's note, this instead PASSES today — a characterization pin.)

- [ ] **Step 3: Rewrite `active_grab_target` to read the record**

```rust
fn active_grab_target(
    state: &ServerState,
) -> Option<(ResourceId, ClientId, i32, i32, bool, bool, u32)> {
    let grab = state.active_pointer_grab?;
    let target = client_target_id(state, grab.owner)?;
    let (gx, gy) = state.resources.window_absolute_position(grab.grab_window);
    Some((
        grab.grab_window,
        target,
        gx,
        gy,
        grab.owner_events,
        grab.via_xi2,
        u32::from(grab.event_mask),
    ))
}
```

- [ ] **Step 4: Run test + grab suite**

Run: `cargo test -p yserver-core active_grab_target_uses_the_matched_passive_grab_not_last_registered && cargo test -p yserver-core`
Expected: the matched-grab test now PASSES; the full suite stays green (the matched-vs-last-registered change is intentional and pinned; no OTHER routing regression).

- [ ] **Step 5: Commit**

```bash
git add crates/yserver-core/src/core_loop/pointer_fanout.rs
git commit -m "refactor(grab): active_grab_target reads the unified record"
```

---

## Task 5: Migrate the remaining passive-keyed readers to the record

Redirect the readers that key on `pointer_grab_is_passive` / `pointer_grab` for passive behavior onto `active_pointer_grab`. Each is a small, individually-testable swap; run the grab suite after each.

**Files & swaps** (each: replace the legacy read with the record equivalent):
- `pointer_fanout.rs:1222-1233` XI2 passive gate: `state.pointer_grab_is_passive` → `state.active_pointer_grab.is_some_and(|g| g.passive)`; `state.pointer_grab` owner → `g.owner`.
- `pointer_fanout.rs:812` `active_grab_present` → `state.active_pointer_grab.is_some()`.
- `pointer_fanout.rs:1661` implicit-install guard `state.pointer_grab.is_some()` → `state.active_pointer_grab.is_some()`.
- `pointer_fanout.rs:2596-2597` release guard → `g.passive` from the record.
- `pointer_fanout.rs:2998` barrier `.is_some()` → record.
- `process_request.rs:13915, 20034, 20145` AllowEvents branch selectors → `g.passive` / `g.owner`.
- `process_request.rs:11903, 11931, 24160, 24756` owner filters → `g.owner`.
- `process_disconnect.rs:330` → `active_pointer_grab.is_some_and(|g| g.owner == client)`.
- `server.rs` legacy `pointer_event_fanout_inner` — **production-dead but TEST-exercised** (codex finding 4). Verified: its only callers are `pointer_event_fanout`/`route_button_press_no_grab` at `server.rs:2465/2477`, and every call site is under `#[cfg(test)]` (module starts `:3057`); no binary path reaches it (the live pointer path is `pointer_event_fanout_to_state`). BUT it is a *parallel* passive-grab implementation with its OWN write and read sites that field deletion (Task 9) will break, so it must be **fully migrated, not skipped**:
  - passive activation write `:2690-2691` → also `set_pointer_grab(...)` with a full record (mirror Task 2), snapshotting the matched grab.
  - passive release write `:2650-2651` → also `clear_pointer_grab()` (mirror Task 3).
  - reads `:2540, 2543, 2550` → read the record.
  Its correctness is covered by the EXISTING server.rs grab tests that drive this path (`replay_pointer_delivers_to_button_press_window_not_grab_owner:3719`, `passive_grab_owner_events_*:3868/4018`) — they must stay green after migration. Do NOT delete the path (that would drop those tests' coverage); migrate it.

- [ ] **Step 1: Characterization test for the XI2 passive gate**

The subtlest reader is the XI2 synchronous passive-grab freeze gate (`:1222`). Pin it before the swap:

```rust
#[test]
fn xi2_passive_freeze_gate_uses_record_passive_flag() {
    // A synchronous XI2 passive grab, frozen with a stored press, must
    // still funnel the XI2 press to the grab owner only. Assert the
    // xi2_targets are filtered to the grab owner. (Model after the
    // existing xi_sync_passive_grab_* tests; read one for the setup.)
}
```

Read `xi_sync_passive_grab_replays_xi2_press_to_target_only_after_allow_events` (`process_request.rs:35939`) for the exact freeze/stored setup and reuse it.

- [ ] **Step 2: Run — PASS today** (characterization).

Run: `cargo test -p yserver-core xi2_passive_freeze_gate_uses_record_passive_flag`

- [ ] **Step 3: Apply the swaps** listed above, one file at a time.

For `:1222-1233`:

```rust
    if handle_grabs
        && event.kind == PointerEventKind::ButtonPress
        && state.active_pointer_grab.is_some_and(|g| g.passive)
        && state
            .xi1_frozen
            .get(&crate::xinput::DEVICEID_SLAVE_POINTER)
            .is_some_and(|freeze| freeze.stored.is_some())
        && let Some(grab_owner) = state.active_pointer_grab.map(|g| g.owner)
    {
        xi2_targets.retain(|cid| *cid == grab_owner);
        xi2_grab_delivery = true;
    }
```

- [ ] **Step 4: Run the full suite after each file**

Run: `cargo test -p yserver-core`
Expected: PASS at every step. If any test flips, STOP — the record and legacy fields disagree for that path; fix the writer before continuing.

- [ ] **Step 5: Commit** (one commit per file is fine)

```bash
git add crates/yserver-core/src/core_loop/
git commit -m "refactor(grab): migrate passive-keyed readers to the unified record"
```

---

## Task 6: Xorg-faithful implicit-grab attribution (#5) — reframe the MATE e2e test

This is the correctness crux. Change which delivery is credited as the implicit-grab owner to match Xorg's deepest-window / XI2-first rule.

**Files:**
- Modify: `crates/yserver-core/src/core_loop/pointer_fanout.rs` (`DeliveredPress` capture at `:1060` and `:1599`, and/or the selection in `implicit_pointer_grab_lifecycle`)
- Modify (reframe): `crates/yserver-core/src/core_loop/process_request.rs:48910` `mate_combo_grab_succeeds_after_core_first_implicit_grab_end_to_end`

- [ ] **Step 1: Reframe the e2e MATE test to the Xorg-faithful invariant**

The current precondition asserts the *bug* (core ancestor owns the implicit grab). Rewrite the two grab-ownership assertions (`:49003-49009` and `:49037-49042`) to assert the **app** owns the implicit grab and the grab succeeds via SameClient:

```rust
        // Xorg-faithful (../xserver dix/events.c:2885 DeliverDeviceEvents:
        // XI2 delivered first at the DEEPER window). The leaf carries the
        // app's XI2 selector, so the implicit grab is owned by the APP, not
        // the core ancestor on the parent.
        assert_eq!(
            state.active_pointer_grab.map(|g| g.owner),
            Some(ClientId(APP)),
            "implicit grab is attributed to the deeper XI2 window's client (the app)",
        );
        assert!(state.active_pointer_grab.is_some_and(|g| g.implicit));
        let _ = read_all_available(&mut app_peer);
```

and after the XIGrabDevice:

```rust
        assert_eq!(
            state.active_pointer_grab.map(|g| (g.owner, g.grab_window)),
            Some((ClientId(APP), combo)),
            "the app's XIGrabDevice is SameClient (it already owned the implicit \
             grab) → replaces → Success, no supersede needed",
        );
        let reply = read_all_available(&mut app_peer);
        assert!(reply.len() >= 9 && reply[0] == 1 && reply[8] == 0,
            "combo grab reply must be Success (0); got {:?}", &reply[..reply.len().min(12)]);
```

Rename the test to `mate_combo_implicit_grab_attributed_to_app_leaf_xi2` and update its doc comment to cite the Xorg walk.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p yserver-core mate_combo_implicit_grab_attributed_to_app_leaf_xi2`
Expected: FAIL — yserver currently attributes to `CORE_ANCESTOR` (core-first).

- [ ] **Step 3: Implement deepest-window / XI2-first attribution**

The core fanout captures `delivered_press` at `:1060` and the XI2 fanout at `:1599`, each gated `delivered_press.is_none()` (core wins). Change to capture **both** candidates and select the deeper, XI2 winning ties. Concretely, replace the single `delivered_press: Option<DeliveredPress>` with two candidates during fanout and resolve at the end:

Add to `ImplicitGrabFanoutInfo`:
```rust
    core_press: Option<DeliveredPress>,
    xi2_press: Option<DeliveredPress>,
```
Remove `delivered_press` (or make it a computed accessor). At `:1060`, populate `core_press` (guard `core_press.is_none()`). At `:1599`, populate `xi2_press` (guard `xi2_press.is_none()`). Then add a resolver used by `implicit_pointer_grab_lifecycle`:

```rust
impl ImplicitGrabFanoutInfo {
    /// Xorg DeliverDeviceEvents (dix/events.c:2885): the deepest window
    /// with any delivery wins; within a window XI2 precedes core. yserver
    /// runs core and XI2 as separate leaf→root propagations of the SAME
    /// physical hit, so the two candidate windows are always on one
    /// ancestor chain — one is an ancestor of (or equal to) the other.
    /// Pick the strictly-deeper window; equal window ⇒ XI2 (Xorg tries
    /// XI2 first at a given window). "Unrelated" is impossible for a
    /// single hit; if it happens, candidate capture is buggy — assert
    /// rather than silently guess.
    fn delivered_press(&self, res: &crate::resources::Resources) -> Option<DeliveredPress> {
        match (self.core_press, self.xi2_press) {
            (Some(c), Some(x)) => {
                if x.window == c.window {
                    Some(x)               // same window ⇒ XI2 first (Xorg order)
                } else if res.is_descendant_of(x.window, c.window) {
                    Some(x)               // XI2 window strictly deeper
                } else if res.is_descendant_of(c.window, x.window) {
                    Some(c)               // core window strictly deeper
                } else {
                    debug_assert!(
                        false,
                        "implicit-grab candidates on unrelated windows (core={:?} xi2={:?}) \
                         — both derive from one hit and must share an ancestor chain",
                        c.window, x.window,
                    );
                    Some(x)               // defensive: XI2 (Xorg's first-tried level)
                }
            }
            (Some(c), None) => Some(c),
            (None, Some(x)) => Some(x),
            (None, None) => None,
        }
    }
}
```

The `same/unrelated ⇒ XI2` tie-break was wrong in the first draft (codex finding 1): a single hit cannot produce unrelated candidate windows, so that case is a bug, not an Xorg rule.

Update `implicit_pointer_grab_lifecycle` (`:1658`) to call `info.delivered_press(&state.resources)` instead of reading the field, and update its `via_xi2`/`core_mask`/`xi2_mask` usage accordingly (already carried on `DeliveredPress`). The signature already receives `state`, so `&state.resources` is in scope.

- [ ] **Step 4: Run the reframed test + the XI2-attribution suite**

Run: `cargo test -p yserver-core mate_combo_implicit_grab_attributed_to_app_leaf_xi2`
Then: `cargo test -p yserver-core implicit_grab`
Expected: reframed test PASS. If `implicit_grab_core_release_follows_press_recipient` / `implicit_grab_xi2_release_follows_press_recipient` flip, re-derive them against the deepest-window rule (they should still hold — "release follows press recipient" is the same principle — but revalidate each assertion against Xorg, do not just edit to green).

- [ ] **Step 5: Pin the 440ad099 interaction as an ACCEPTANCE check (codex direct answer), then full suite**

The attribution flip makes the app-owned implicit grab `via_xi2=true`, so the `core_implicit = implicit && !via_xi2` gate at `:1262` stops applying to it. Codex's read: this is not obviously broken — the XI2 redirect still delivers the release via `xi2_mask` at `:1286`. But do NOT leave it as an ad-hoc "confirm": the existing XFCE-dialog test (`implicit_grab_xi2_release_follows_press_recipient` and any `xfce`-oracle test that pins "13 XI2 presses → 3 XI2 releases" from `xfce.xtrace`) is the pinned gate. Identify that test by name (grep the tests for `xfce`/`core_implicit`/`release_follows`), and REQUIRE it green here — it is the acceptance criterion, not a note. If it flips, the release-delivery for a now-`via_xi2` implicit grab regressed; fix delivery, do not revert attribution.

Run: `cargo test -p yserver-core`
Expected: PASS, including the XFCE-dialog release-count test.

- [ ] **Step 6: Commit**

```bash
git add crates/yserver-core/src/core_loop/
git commit -m "fix(grab): implicit-grab owner = deepest window, XI2-first (Xorg DeliverDeviceEvents)"
```

---

## Task 7: Passive/implicit lifetime (#6) — Cinnamon acceptance pin + final-release fix

**Framing (codex round-1 + round-2):** the replay-teardown paths ALREADY exist (`apply_allow_events` `process_request.rs:20145`, XIAllowEvents `:13915`, both routing through `deactivate_passive_pointer_grab_crossings`). So this task is **"verify the existing replay teardown clears the record, and fix the one confirmed early-release divergence"** — NOT "add replay deactivation." The confirmed divergence: `release_passive_grab_on_button_release` (`pointer_fanout.rs:2596`) releases on *any* ButtonRelease, whereas Xorg releases only on the FINAL release with `buttons_down == 0` (`../xserver/Xi/exevents.c:1935`).

Because Task 3 already makes `deactivate_passive_pointer_grab_crossings` call `clear_pointer_grab()`, the Cinnamon acceptance test likely **goes green after Task 3, not RED** (codex round-2 medium finding). So this task has TWO distinct pieces: a Cinnamon acceptance PIN (may already be green) and a genuinely-RED final-release fix. Do not mandate the Cinnamon test be RED.

### Trace grounding (corrected per codex round-2; independently re-verified this session)

**Client identities in `cinnamon.xtrace`:** conn **029** is muffin (`_MUFFIN_FOCUS_SET`, `cinnamon.xtrace:4855`) and does only focus-setting + 4 `XIUngrabDevice`. Conn **026** is the **Cinnamon shell** (`WM_CLIENT_MACHINE='bee'` on window `0x00e00011`; the earlier draft wrongly called 026 "muffin"). Conn 026 is the client that actually drives input grabs: **266 `XIPassiveGrabDevice`, 5 `XIAllowEvents`, 9 `XIGrabDevice`**. Its `XIAllowEvents` at seq `03cf`/`03db`/`1151` are **deviceid=2, mode=2 = XIReplayDevice** (body `xXIAllowEventsReq`: time[4] deviceid[2] mode[1] pad[1]; XI2 modes 0=Async,1=Sync,2=Replay); a later cluster (`:79205`) is mode=1 (SyncDevice). **No client issues `XIAllowEvents` on device 3.**

**What the trace grounds (and what it does NOT):** it confirms the *mechanism* is exercised — a client holds a device-2 passive grab and tears it down with `XIReplayDevice` — and that no device-3 AllowEvents occurs. It does NOT cleanly isolate a single "foreign grab blocks the app → replay → app grab succeeds" choreography across the 026/app/029 clients. Therefore the deterministic test's correctness rests on the **Xorg SOURCE** (`../xserver/dix/events.c:1898` ReplayDevice→NOT_GRABBED→DeactivatePointerGrab; `Xi/exevents.c:1935` final-release), which is the de-facto spec — the trace is corroboration, not the oracle. Consequently the RED Cinnamon test's hand-set foreign **keyboard** grab precondition (`:48749-48755`) is unsupported (no device-3 teardown exists) and is DROPPED.

**Files:**
- Modify (reframe): `process_request.rs:48708` `xi_dialog_grab_succeeds_over_foreign_passive_grab_cinnamon`
- Modify (the RED divergence): `release_passive_grab_on_button_release` (`pointer_fanout.rs:2596`)

### Part A — Cinnamon acceptance pin (regression PIN; expected GREEN after Task 3)

- [ ] **Step A1: Reframe the Cinnamon test to the source-grounded sequence**

Pointer flow: activate a device-2 passive grab (populate the record via the Task 2 path — do NOT hand-set the `active_pointer_grab = None` desync), then issue `XIAllowEvents` **deviceid=2 mode=2 (ReplayDevice)** which must deactivate the passive grab, then the app's device-2 `XIGrabDevice` returns Success. Drop the hand-set foreign keyboard grab; assert the device-3 grab succeeds against **no** foreign keyboard grab. Model the AllowEvents call on `xi_allow_events_replay_device_*` (`process_request.rs:34433, 34755`). Remove `#[ignore]`; keep the test name (acceptance gate); doc-comment cites `../xserver/dix/events.c:1898` as the invariant source and the `cinnamon.xtrace` conn-026 ReplayDevice as corroboration (with the corrected client label).

Add the anti-false-green assertion (codex round-1 finding 2) BETWEEN the ReplayDevice call and the app's grab — this is what makes the pin meaningful while the supersede exemption is still live (until Task 8):

```rust
        // ReplayDevice must have DEACTIVATED the foreign passive grab
        // (Xorg NOT_GRABBED, dix/events.c:1898) — so the app's Success
        // proves lifetime, NOT the still-live supersede hack. GRAB_OWNER
        // is the client that held the passive grab (the reframed test's
        // stand-in for the Cinnamon shell).
        assert!(
            !state.active_pointer_grab.is_some_and(|g| g.owner == ClientId(GRAB_OWNER)),
            "foreign passive pointer grab must be gone after ReplayDevice",
        );
        assert!(
            !state.active_keyboard_grab.is_some_and(|g| g.owner == ClientId(GRAB_OWNER)),
            "no foreign keyboard grab is held at grab-time (trace: no device-3 AllowEvents)",
        );
```

- [ ] **Step A2: Run — expected GREEN once Task 3 is in**

Run: `cargo test -p yserver-core xi_dialog_grab_succeeds_over_foreign_passive_grab_cinnamon`
Expected: PASS. The existing ReplayDevice path (`:20145`) routes through `deactivate_passive_pointer_grab_crossings`, which Task 3 made clear the record; so the passive grab is gone and the app grab succeeds. **If it FAILS at the anti-false-green assertion**, the ReplayDevice path does NOT deactivate an XI2 (`via_xi2`) passive grab — fix that path to deactivate on `NOT_GRABBED` (cite `../xserver/dix/events.c:1898`); do NOT relax the guard to compensate.

### Part B — Final-release Xorg alignment (genuinely RED)

- [ ] **Step B1: Write the RED test — a NON-final release must not deactivate**

```rust
#[test]
fn passive_grab_survives_non_final_button_release() {
    // Xorg Xi/exevents.c:1935 deactivates a from-passive/implicit grab
    // only when buttons_down == 0 after the release. yserver releases on
    // ANY ButtonRelease (pointer_fanout.rs:2596). Press button 1 then
    // button 3 (buttons_down==2), release button 1 (buttons_down==1):
    // the passive grab must STILL be active.
    // ... passive-grab setup (as Task 2) ...
    // fanout: ButtonPress(1), ButtonPress(3), ButtonRelease(1)
    assert!(state.active_pointer_grab.is_some_and(|g| g.passive),
        "passive grab must survive a non-final release (buttons still down)");
}
```

- [ ] **Step B2: Run to verify it FAILS**

Run: `cargo test -p yserver-core passive_grab_survives_non_final_button_release`
Expected: FAIL — the grab is cleared on the button-1 release even though button 3 is still down.

- [ ] **Step B3: Gate the release on `buttons_down == 0`**

In `release_passive_grab_on_button_release` (`pointer_fanout.rs:2596-2600`), add the `buttons_down == 0` condition (mirror `implicit_pointer_grab_lifecycle:1691-1692`), citing `../xserver/Xi/exevents.c:1935`:

```rust
fn release_passive_grab_on_button_release(state: &mut ServerState, kind: PointerEventKind) {
    if kind == PointerEventKind::ButtonRelease
        && state.buttons_down == 0
        && state.active_pointer_grab.is_some_and(|g| g.passive)
    {
```

(Confirm `buttons_down` is decremented BEFORE this runs in the fanout order; if not, adjust the condition to the post-decrement count.)

- [ ] **Step B4: Run + full suite**

Run: `cargo test -p yserver-core passive_grab_survives_non_final_button_release && cargo test -p yserver-core`
Expected: PASS. If a single-button passive-release test flips, confirm `buttons_down` timing.

- [ ] **Step B5: Commit**

```bash
git add crates/yserver-core/src/core_loop/process_request.rs crates/yserver-core/src/core_loop/pointer_fanout.rs
git commit -m "fix(grab): passive grab lifetime matches Xorg (final-release buttons_down==0 + ReplayDevice pin)"
```

---

## Task 8: Pure-Xorg guard + remove the `3312e24e` supersede exemption (LAST)

Now that attribution (#6/Task 6) and lifetime (#7) are correct, remove the supersede exemption so the guard is pure-Xorg: any foreign active grab → AlreadyGrabbed; SameClient → replace.

**Files:**
- Modify: `crates/yserver-core/src/core_loop/process_request.rs:11657-11676` (XIGrabDevice guard)
- Modify (reframe): `process_request.rs:48813` `xi_grab_device_supersedes_foreign_implicit_grab_mate_combo`

- [ ] **Step 1: Reframe the supersede unit test to the SameClient invariant**

`xi_grab_device_supersedes_foreign_implicit_grab_mate_combo` hand-sets a foreign implicit grab owned by `CORE_ANCESTOR` and asserts the app's grab supersedes it. Under the faithful model this scenario can't arise from a real click (Task 6 attributes to the app), and the *unit* invariant is now: a foreign implicit grab held by another client is a **real** grab → `AlreadyGrabbed` (pure guard). Rewrite it to assert that, and rename to `xi_grab_device_over_foreign_implicit_grab_returns_already_grabbed`. The MATE combo *behavior* is covered by the e2e test (Task 6) which no longer relies on supersede.

```rust
        // Pure-Xorg guard (dix/events.c:5240): a grab held by ANOTHER client
        // — implicit included — yields AlreadyGrabbed. The MATE combo works
        // via correct attribution (app owns its own implicit grab), covered
        // by mate_combo_implicit_grab_attributed_to_app_leaf_xi2.
        let reply = read_all_available(&mut app_peer);
        assert!(reply.len() >= 9 && reply[0] == 1 && reply[8] == 1,
            "grab over a FOREIGN implicit grab is AlreadyGrabbed(1); got {:?}",
            &reply[..reply.len().min(12)]);
        assert_eq!(state.active_pointer_grab.map(|g| g.owner), Some(ClientId(CORE_ANCESTOR)),
            "the foreign implicit grab is untouched");
```

- [ ] **Step 2: Run — FAIL under current supersede code**

Run: `cargo test -p yserver-core xi_grab_device_over_foreign_implicit_grab_returns_already_grabbed`
Expected: FAIL — current guard exempts implicit grabs (returns Success).

- [ ] **Step 3: Remove the supersede exemption**

`process_request.rs:11657-11676` — replace the `else` branch body with the pure guard:

```rust
            } else {
                // Xorg dix/events.c:5240 GrabDevice: AlreadyGrabbed for ANY
                // foreign active grab (explicit, passive-activated, OR
                // press-driven implicit) — no exemption. The MATE combo and
                // Cinnamon dialog work via correct implicit-grab attribution
                // (Task 6) and Xorg-faithful grab lifetime (Task 7), not by
                // letting an explicit grab bulldoze a foreign grab. SameClient
                // re-grab still replaces (handled below).
                u8::from(
                    state
                        .active_pointer_grab
                        .is_some_and(|g| g.owner != client_id),
                )
            };
```

Delete the `held_by_other` + `held_is_implicit` locals and the `3312e24e` comment block. Also update the GrabPointer guard at `:24040-24043` to read only the record:

```rust
        let grabbed_by_other = state
            .active_pointer_grab
            .is_some_and(|g| g.owner != client_id);
```

- [ ] **Step 4: Run the acceptance gates together**

Run:
```
cargo test -p yserver-core xi_grab_device_over_foreign_implicit_grab_returns_already_grabbed
cargo test -p yserver-core mate_combo_implicit_grab_attributed_to_app_leaf_xi2
cargo test -p yserver-core xi_dialog_grab_succeeds_over_foreign_passive_grab_cinnamon
cargo test -p yserver-core xi_grab_device_by_other_client_must_not_steal_held_grab_cinnamon_xtrace
```
Expected: all PASS. (The Steam #94 clobber test — explicit-vs-explicit AlreadyGrabbed — must stay green.)

- [ ] **Step 5: Full suite**

Run: `cargo test -p yserver-core`
Expected: PASS. Any flip here means attribution or lifetime is still off — do NOT re-add the exemption; fix the root.

- [ ] **Step 6: Commit**

```bash
git add crates/yserver-core/src/core_loop/process_request.rs
git commit -m "fix(grab): pure-Xorg AlreadyGrabbed guard; remove 3312e24e supersede exemption"
```

---

## Task 9: Delete the legacy fields

Every reader/writer is migrated and the dual-write assertions have held across the suite. Remove `pointer_grab` and `pointer_grab_is_passive`.

**Files:**
- Modify: `crates/yserver-core/src/server.rs` (delete fields `:823, :837`, constructor inits `:1196, :1200`)
- Modify: every remaining `pointer_grab` / `pointer_grab_is_passive` write (the legacy half of the dual-writes) and any remaining read; delete the `debug_assert_eq!` desync checks.
- Modify: tests that set/read the legacy fields (census buckets 1/2/4/5) — switch to `active_pointer_grab` / helpers.

- [ ] **Step 1: Delete the field definitions and constructor inits**

Remove the two `pub` fields and their `pointer_grab: None,` / `pointer_grab_is_passive: false,` in `ServerState::with_geometry` (`:1196, :1200`).

- [ ] **Step 2: Fix every compile error**

Run: `cargo build -p yserver-core` and fix each error by removing the legacy write (the record write beside it remains) or switching the read to `active_pointer_grab`. Delete the dual-write `debug_assert_eq!` blocks added in Tasks 2. In tests, replace `state.pointer_grab = Some((c, w)); state.pointer_grab_is_passive = p;` triplets with `state.set_pointer_grab(ActivePointerGrab { .. })` and `state.pointer_grab` reads with `state.active_pointer_grab.map(|g| (g.owner, g.grab_window))`.

- [ ] **Step 3: Build clean**

Run: `cargo build -p yserver-core --all-targets`
Expected: PASS, zero warnings about unused fields/vars.

- [ ] **Step 4: Full suite + clippy + fmt**

Run (clippy/fmt exactly as AGENTS.md/CI require — NOT crate-scoped, or lints in test code are missed):
```
cargo test -p yserver-core
cargo clippy --all-targets -- -D warnings
cargo +nightly fmt
```
Expected: tests PASS, clippy clean at `-D warnings` (plain clippy — pedantic is opt-in per AGENTS.md), fmt no diff after.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "refactor(grab): delete pointer_grab + pointer_grab_is_passive (single source of truth)"
```

---

## Task 10: Per-transition consistency + regression sweep (acceptance)

- [ ] **Step 1: Add a state-consistency invariant test**

With one field there is no desync to assert *between* fields, but pin that every activation path yields a well-formed record and every teardown clears it:

```rust
#[test]
fn every_grab_activation_path_sets_a_consistent_record() {
    // Explicit GrabPointer, XIGrabDevice, passive activation, implicit press:
    // after each, active_pointer_grab is Some with owner == the granting
    // client and the right `passive`/`implicit` flags; after the matching
    // teardown (UngrabPointer / XIUngrabDevice / release / disconnect) it is
    // None. Drive each path and assert. (Compose from the existing per-path
    // tests; this is the single-source-of-truth acceptance pin.)
}
```

- [ ] **Step 2: Run the whole XI + grab surface**

Run: `cargo test -p yserver-core`
Expected: PASS.

- [ ] **Step 3: xts XI regression (project recipe)**

Run the xts XInput suite per `reference_test_recipes` (`just xts-yserver` / the XI subset). Compare against the pre-branch baseline; no new failures.

- [ ] **Step 4: HW smoke where a repro exists**

Per `feedback_hw_recipes_user_only`, the USER runs the HW smoke (one agent per checkout, coordinate first). Cinnamon (xfce-terminal Preferences combobox/checkbox), MATE Mouse Preferences combo, Steam library click, XFCE dialog. Confirm no input wedge. Note: bee can't reproduce the HW race; the deterministic tests are the primary gate (spec Testing §).

- [ ] **Step 5: Final commit / branch status**

```bash
git add -A && git commit -m "test(grab): single-source-of-truth acceptance + consistency pins"
```

Branch is ready for PR review after codex plan-review, implementation, and the HW smoke pass. Per `feedback_no_pr_before_ready` / `feedback_confirm_each_master_push`, do not open the PR or push to master without explicit user confirmation.

---

## Self-review notes

- **Spec coverage:** §1 single record (Task 1), §2 writes-through-one-path (Tasks 2–3, helpers), §3 reads-direct (Tasks 4–5), §4 pure guard (Task 8), §5 attribution (Task 6), §6 lifetime (Task 7), §7 out-of-scope respected (no `button_grabs`/`key_grabs`/`xi1_active_grabs`/keyboard changes beyond the guard). Migration §§1–5 map to Tasks 1→2/3→4/5→(6+7+8 as one series)→9. Testing §§1–4 map to Tasks 7/8/6/10.
- **Refinement flagged:** Task 6 corrects the spec's cited §5 mechanism ("core events delivered first") to Xorg's actual `DeliverDeviceEvents` deepest-window/XI2-first rule, and reframes `mate_combo_..._end_to_end` (currently asserts the bug). This must be called out in codex plan-review.
- **Type consistency:** helper names `set_pointer_grab`/`clear_pointer_grab`, field `passive`, `ImplicitGrabFanoutInfo::{core_press,xi2_press,delivered_press(&Resources)}` used consistently across Tasks 1–9.
- **Open item for the implementer (Task 7):** the exact lingering-release path is now grounded (ReplayDevice, decoded from `cinnamon.xtrace`), but confirm the ReplayDevice→deactivation path fires for an XI2 passive grab during implementation. Verify each `PassiveButtonGrab` field name against `server.rs` before writing the Task 2/3 tests.

## Codex plan-review (round 1, gpt-5.4 high) — findings addressed

All 7 findings verified against the repo/`../xserver` and folded in:
1. **[High] "Xorg-faithful" overclaim + wrong `unrelated⇒XI2` tie-break** → added the "Scope boundary" note (localized core/XI2 approximation; XI1 implicit grabs aren't installed, `pointer_fanout.rs:1818`); resolver now `debug_assert!`s on unrelated candidates (impossible for one hit) and treats equal-window as XI2-first.
2. **[High] Task 7 can false-green before Task 8 removes supersede** → added the pre-grab assertion that the foreign passive grab is actually *cleared* before the app grabs — proves lifetime, immune to the still-live supersede hack. (Round 2 corrected the client label — see below.)
3. **[High] Cinnamon reframe mode unpinned + keyboard half** → decoded `cinnamon.xtrace`: the grab-holding client issues **XIReplayDevice (mode 2) on device 2 only**; the hand-set device-3 keyboard-grab precondition is ungrounded and is dropped (grab succeeds because no foreign keyboard grab is held). **Round 2 correction:** that grab-holding client is conn 026 (the Cinnamon shell), NOT muffin (conn 029) — see the round-2 section below for the corrected attribution.
4. **[Med] `server.rs` fanout mislabeled "dead, skip"** → verified production-dead (all callers `#[cfg(test)]`) but test-exercised; upgraded to a full migration target (populate/clear record in its passive paths, migrate reads; keep its tests).
5. **[Med] Task 4 characterization too weak** → replaced with a matched-vs-last-registered RED test; flagged the `button_grabs` fallback → record change as an intentional behavior change to pin.
6. **[Med] dual-write assert too weak** → now compares `(owner, grab_window)` and the `passive` flag, not just owner.
7. **[Low] spec path typo + clippy/fmt commands** → spec filename corrected; Task 9 now uses `cargo clippy --all-targets -- -D warnings` and `cargo +nightly fmt` per AGENTS.md.

Codex direct answers also folded in: 440ad099 interaction is now a *pinned* XFCE-oracle acceptance check in Task 6 Step 5 (not ad-hoc "confirm"); Task 7 reframed as "fix early release + verify EXISTING replay teardown (`:20145`/`:13915`)", not "add replay deactivation."

## Codex plan-review (round 2, gpt-5.4 high) — findings addressed

Round 2 confirmed **6 of 7** round-1 fixes ADEQUATE (items 1, 2, 4, 5, 6, 7). Two Task-7 problems remained; both fixed:
- **[High] Wrong client attribution in the Cinnamon grounding.** Codex found — and I independently re-verified — that conn `026` is the **Cinnamon shell** (not muffin; muffin is conn `029`, which only focus-sets + 4 `XIUngrabDevice`). The `XIReplayDevice mode=2 device=2` decode is correct, but the client issuing it is conn 026 (266 `XIPassiveGrabDevice`, 5 `XIAllowEvents`). Task 7 rewritten: corrected the client label; grounded the release on the Xorg **source** (`dix/events.c:1898`, the de-facto spec) with the trace as corroboration only; dropped the unsupported device-3 keyboard-grab precondition (no client issues device-3 AllowEvents).
- **[Med] Cinnamon test's "RED first" expectation was wrong.** Replay teardown already exists, and Task 3 makes it clear the record, so the Cinnamon test goes GREEN after Task 3, not RED. Task 7 split into **Part A** (Cinnamon acceptance PIN, expected green post-Task-3, keeps the anti-false-green assertion) and **Part B** (the genuinely-RED final-release fix: a non-final ButtonRelease must not deactivate, gated on `buttons_down == 0` per `Xi/exevents.c:1935`).

Round-2 verdicts on the other items: 1 adequate (unrelated-`debug_assert` locally safe — core candidate walks only the hit ancestor chain, XI2 resolves to hit/target/top-level/ROOT on that same chain); 2 adequate (assertion closes the false-green hole); 4 adequate (production-dead confirmed, all field accesses covered); 5 adequate (Task 2 snapshots the `try_match_passive_grab` result, so the matched-mask test can go green); 6 adequate (`via_xi2`/`implicit` have no legacy counterpart to compare); 7 adequate.
