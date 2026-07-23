# Freeze-State Unification Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Collapse the two parallel representations of synchronous-grab freeze state (the legacy server-scope core slots `frozen_pointer_event` / `frozen_pointer_queue` / `frozen_keyboard_event` vs the per-device `Xi1Freeze` record) into a single source of truth matching Xorg's `GrabInfoRec.sync` + global `syncEvents.pending` model, and fix the two residual conformance gaps recorded in `docs/known-issues.md` (2026-07-15).

**Architecture:** Follow Xorg exactly. Per-device `Xi1Freeze` keeps the sync `state`, `other`, and one activating event (`stored`, Replay material only). All withheld/released events move to ONE global device-tagged queue (`ServerState.sync_pending`) replayed by a single backend-aware `xi1_compute_freezes` that ports Xorg's `ComputeFreezes` → `PlayReleasedEvents` restart-from-head loop, protected by a RAII `playing_sync_events` guard (Xorg `syncEvents.playingEvents`). Because yserver withholds events in **three** forms — core-form `HostPointerEvent` / `HostKeyEvent` (core + XI2 fanout, host coords) and XI1-form `Xi1QueuedEvent` (XI1 explicit device grabs, carrying `deviceid`/`evcode`/`focus_route`/`axes`/`replay_floor`) — the queue and the `stored` slot hold a tagged union `QueuedInputEvent` that preserves each form and replays it through its correct path. The AllowEvents admission gate becomes `(this_grabbed && state >= FrozenNoEvent) || this_synced` — unified state is the sole authority.

**Tech Stack:** Rust (`crates/yserver-core`); in-crate unit tests using `ServerState::new()` + `install_client` (peer sockets, wire-order assertions) + `RecordingBackend` (backend calls only — it does NOT record client event delivery; see Test Conventions); Xorg reference at `../xserver/dix/events.c` and `../xserver/include/inputstr.h`.

**Toolchain (from AGENTS.md — overrides global pedantic preference for this repo):**
- Format: `cargo +nightly fmt`
- Lint (exactly as CI, run before EVERY commit): `cargo clippy --all-targets -- -D warnings` — CI fails on any warning, and `--all-targets` lints test code too. No task may leave an intermediate state with warnings; there is no "warnings acceptable" step.
- Test: `cargo test -p yserver-core`
- Spec compliance is the goal, but where Xorg deviates from spec, follow Xorg.

**Test Conventions (verified against the crate):**
- `RecordingBackend` (`crates/yserver-core/src/backend/recording.rs`) records **backend** operations, NOT client event delivery. `deliver_routed_key` / `pointer_event_fanout_to_state` write X events to client **peer sockets**.
- To assert an event was delivered to a client, use the `install_client` pattern (see `xi_allow_events_async_device_thaws_freeze_and_replays_queue`, `process_request.rs:34591`): create a client with a nonblocking peer socket, select the event mask, dispatch, then read the wire bytes from the peer and assert on event type/order. Cross-device ordering tests MUST read two clients' wires and compare, or read one client selecting both device event types and assert byte order.

**Xorg reference anchors (read before starting):**
- `../xserver/include/inputstr.h:504-532` — `GrabInfoRec.sync` = `{ state, other, event, evcount }`; `FROZEN`(5)/`FROZEN_NO_EVENT`(5)/`FROZEN_WITH_EVENT`(6).
- `../xserver/include/inputstr.h:655-688` — global `EventSyncInfoRec syncEvents` = `{ pending, playingEvents, replayDev, replayWin, time }`.
- `../xserver/dix/events.c:1233-1291` — `PlayReleasedEvents`: walk global `pending`, replay each event whose device is unfrozen, **restart from head** after each replay.
- `../xserver/dix/events.c:1319-1374` — `ComputeFreezes`: derive `sync.frozen` per device, replay `replayDev`'s `sync.event` (via `CheckDeviceGrabs`/`DeliverDeviceEvents`), then `PlayReleasedEvents` iff any device unfrozen; guarded by `playingEvents`.
- `../xserver/dix/events.c:1823-1934` — `AllowSome`: gate `:1851` = `(thisGrabbed && sync.state >= FROZEN) || thisSynced`; every mode mutates state then calls `ComputeFreezes()`; only `NOT_GRABBED`/Replay (`:1898-1906`) replays `sync.event` and calls `DeactivateGrab`.
- `../xserver/dix/events.c:1220-1221`, `:4435-4447` — Xorg stores the **already-processed** event into `pending`/`sync.event`; replay re-enters `processInputProc`, never re-cooks the physical event.

**Design decisions locked in (Claude analysis + codex design review + codex plan review, 2026-07-15):**
1. **Preserve every event form.** The unified queue/slot type is a tagged union, NOT a lossy host-only struct:
   ```rust
   pub enum QueuedInputEvent {
       HostPointer(crate::host_x11::HostPointerEvent), // core/XI2 pointer, host coords
       HostKey(crate::host_x11::HostKeyEvent),         // core/XI2 key
       Xi1Routed(Xi1QueuedEvent),                      // XI1 device-grab, full routing metadata
   }
   ```
   `Xi1QueuedEvent` (`server.rs:447`) carries `deviceid/evcode/focus_route/axes/replay_floor/natural_target` — none of it reconstructible from a host event, so it must be stored verbatim.
2. **Canonical cook position.** `translate_host_event` (pointer_fanout.rs:2177) recomputes `root_x/y` from `event_x/y` + window pos → idempotent, safe to re-run on replay. But button-mapping (`pointer_fanout.rs:92-103`) mutates `event.detail` and is NOT guarded by `is_replay`, so a stored **post-map** pointer event double-maps on replay. Store the **pre-button-map (physical) detail**. A physical→logical map of 0 returns early at `:99-101` and never reaches the freeze gate — harmless, no vanished event is ever queued (Task 6 states this + adds a non-identity-mapping regression test).
3. **`stored` (activating) vs pending (released) stay distinct.** `stored` replays only for Replay modes (Xorg `:1899`); the global queue replays for all thaw modes. `stored` also drives `DeactivateGrab` on Replay (Xorg `:1904`).
4. **Global pending queue, device-tagged.** Per-device queues reorder cross-device events under SyncBoth. One global queue + Xorg restart-from-head loop.
5. **RAII re-entrancy guard.** `playing_sync_events` is set/cleared by a scope guard so it resets on every exit path (early return, panic-unwind). Update all sync states before replay; pop an event out of the queue before delivering; never hold a `&mut` into a freeze record across a fanout call.
6. **Issue-1 interim fix = fixtures, not gate.** The 3 tuned replay tests break because they set only the legacy slot; fix them by constructing the matching unified `Xi1Freeze` state.
7. **Cleanup is not blanket-drop.** Disconnect/ungrab paths must distinguish device-destroy (discard that device's pending) from grab-deactivate (Xorg thaws the surviving device and REPLAYS its pending — `:1233-1291`). A blanket `retain(device != d)` reintroduces Issue 2 through the ungrab path.

---

## File Structure

- `crates/yserver-core/src/server.rs` — `ServerState` fields + `Xi1Freeze` + new `QueuedInputEvent` / `PendingSyncEvent` types + the `playing_sync_events` guard flag + a `SyncReplayGuard` RAII helper.
- `crates/yserver-core/src/core_loop/pointer_fanout.rs` — QUEUE-WHILE-FROZEN gate (~:470), `xi1_compute_freezes` (~:1953), `xi1_thaw_device` (~:1940), replay helpers (~:70), the XI1 `queue` writer (`xi1_route_device_event`, ~:1632) and XI1 replay/activation (~:1700-1795).
- `crates/yserver-core/src/core_loop/key_fanout.rs` — keyboard withhold gate + `core_key_queue` writer (~:140), `deliver_routed_key` (~:194).
- `crates/yserver-core/src/core_loop/process_request.rs` — `apply_allow_events` (~:19814); XI1 AllowSome port (~:13692); `stored` reader at `:13781`; the tuned + pinning tests (~:34000+); explicit-grab `core_key_queue` write at `:36329`.
- `crates/yserver-core/src/core_loop/process_disconnect.rs` — slot cleanup + `xi1_thaw_device` caller (~:335, ~:386).

**Every `xi1_thaw_device` / `xi1_compute_freezes` caller** (grep both; the verified list):
- `xi1_compute_freezes`: `pointer_fanout.rs:2028, :2089, :2115`; `process_request.rs:13759, :13769, :13804, :13821, :13845, :20114`.
- `xi1_thaw_device`: all callers incl. `process_disconnect.rs:386`, `process_request.rs:19270` (both omitted from any hand list — grep to be exhaustive).

**Writers that MUST migrate to the global queue before the per-device queues are deleted (Task 4a):**
- `Xi1Freeze.queue`: `pointer_fanout.rs:1632` (`xi1_route_device_event`).
- `core_key_queue`: `key_fanout.rs:140`; `process_request.rs:36329` (explicit-grab test path).
- XI1 activating/`stored` write + replay: `pointer_fanout.rs:1700-1795`, `:1764`, `:2060-2065`; XI-form `stored` reader `process_request.rs:13781`.
- Core slot writers: `pointer_fanout.rs:849`; `server.rs:2653`.
- Core slot clearers: `server.rs:2623-2624`; `process_disconnect.rs:335-336`; `process_request.rs:11660`, `:11815-11816`, `:24047-24048`.

---

## Task 1: Pin the Issue-1 conformance gap as a failing test (no placeholders)

Establish a compile-valid, `#[ignore]`d test encoding the Xorg-correct admission behavior. **No `todo!()` and no empty-bodied placeholder in committed code.** Only the Issue-1 pin is authored here; the Issue-2 pin needs behavior that does not exist until Task 4b and is authored there (see note after Step 1).

**Files:**
- Test: `crates/yserver-core/src/core_loop/process_request.rs` (test module, after `:34591`)

- [ ] **Step 1: Write the Issue-1 pinning test in full**

Construct a pointer thawed in unified state but with a lingering legacy slot, this client grabbing; send XI2 `ReplayDevice`; assert the grab client's peer receives no ButtonPress/Release. Use the `install_client` + peer-read pattern from `:34591` (create client, nonblocking peer, select mask, dispatch, read wire bytes).

```rust
/// ISSUE 1 (known-issues.md 2026-07-15): after an out-of-band unified thaw,
/// a lingering legacy `frozen_pointer_event` slot must NOT admit a stale
/// ReplayDevice. Xorg AllowSome gates only on `sync.state >= FROZEN`
/// (dix/events.c:1851). Green after Task 5 (gate drops `core_frozen`).
#[test]
#[ignore = "pins Issue 1; green after Task 5"]
fn xi_allow_events_replay_no_op_when_unified_thawed_despite_stale_core_slot() {
    use crate::{
        host_x11::{HostPointerEvent, PointerEventKind},
        server::{Xi1Freeze, Xi1SyncState},
    };
    const GRAB_CLIENT_ID: u32 = 1;
    const GRAB_WIN: u32 = 0x0010_0061;

    let mut state = ServerState::new();
    let mut grab_peer = install_client(&mut state, GRAB_CLIENT_ID);
    grab_peer.set_nonblocking(true).expect("nonblocking");
    let mut backend = RecordingBackend::new();
    // Grab window + ButtonPress|ButtonRelease mask so a (wrongful) replay would be visible.
    state.resources.create_window(
        ClientId(GRAB_CLIENT_ID),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24, window: ResourceId(GRAB_WIN), parent: ROOT_WINDOW,
            x: 0, y: 0, width: 100, height: 100, border_width: 0, class: 1,
            visual: crate::resources::ROOT_VISUAL, ..Default::default()
        },
    );
    let _ = state.resources.map_window(ResourceId(GRAB_WIN));
    state.clients.get_mut(&GRAB_CLIENT_ID).unwrap()
        .event_masks.insert(ResourceId(GRAB_WIN), 0x0000_000c);

    state.pointer_grab = Some((ClientId(GRAB_CLIENT_ID), ResourceId(GRAB_WIN)));
    state.pointer_grab_is_passive = true;
    // Unified state THAWED out-of-band; legacy slot lingers.
    state.xi1_frozen.insert(
        crate::xinput::DEVICEID_SLAVE_POINTER,
        Xi1Freeze { state: Xi1SyncState::Thawed, ..Default::default() },
    );
    state.frozen_pointer_event = Some(HostPointerEvent {
        kind: PointerEventKind::ButtonPress, host_xid: 0xCAFE_0001, detail: 1,
        time: 0x1000, root_x: 5, root_y: 5, event_x: 5, event_y: 5,
        state: 0, crossing_mode: 0, child: 0,
    });

    // XIAllowEvents XIReplayDevice on master pointer (deviceid=2).
    // XI2 wire mode: AsyncDevice=0, SyncDevice=1, ReplayDevice=2,
    // AsyncPairedDevice=3 (verified xi2_allow_mode_to_core, process_request.rs:19788).
    let mut body = Vec::with_capacity(8);
    body.extend_from_slice(&0u32.to_le_bytes()); // CurrentTime
    body.extend_from_slice(&2u16.to_le_bytes()); // deviceid
    body.push(2); // XIReplayDevice
    body.push(0);
    let header = yserver_protocol::x11::RequestHeader { opcode: 131, data: 53, length_units: 3 };
    handle_xi2_request(&mut state, &mut backend, None, ClientId(GRAB_CLIENT_ID),
        SequenceNumber(1), header, &body).expect("allow events");

    // Grab client must have received NO button event.
    let mut buf = [0u8; 4096];
    let n = grab_peer.read(&mut buf).unwrap_or(0);
    assert_eq!(n, 0, "ReplayDevice on a unified-thawed device must be a no-op (no stale replay)");
}
```

> **The Issue-2 pin is NOT authored here.** It requires `xi1_thaw_device` to replay (backend-aware `xi1_compute_freezes` over the global queue), which does not exist until Task 4b. Authoring an empty-bodied ignored stub now would be a committed placeholder (the thing this plan forbids). The Issue-2 pin is written *in full, with real asserts,* in Task 4b Step 3 — the task where its behavior first exists and passes. Task 1 commits only the Issue-1 pin.

- [ ] **Step 2: Verify the Issue-1 pin compiles and is ignored**

Run: `cargo test -p yserver-core xi_allow_events_replay_no_op_when_unified_thawed -- --ignored --list`
Expected: the test is listed under ignored.

Run: `cargo clippy --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 3: Commit**

```bash
git add crates/yserver-core/src/core_loop/process_request.rs
git commit -m "test(freeze): pin Issue 1 (stale-replay after out-of-band thaw), ignored"
```

---

## Task 2: Add the RAII re-entrancy guard (no signature change, no behavior change)

Insert the `playing_sync_events` re-entrancy guard into the EXISTING `xi1_compute_freezes` with **no signature change and no behavior change**. The signature change (adding `backend`/`xid_map`) is deferred to Task 4b, where those params are actually *used* — adding them here would leave them unused and fail `cargo clippy --all-targets -- -D warnings`, and delivering through the fanout here would change replay semantics (double-mapping legacy post-map events; see Task 6). Task 2 is purely the guard.

**Files:**
- Modify: `crates/yserver-core/src/server.rs` (guard flag + `SyncReplayGuard`)
- Modify: `pointer_fanout.rs:1953` (`xi1_compute_freezes` — insert guard only)

- [x] **Step 1: Add the guard flag + borrow-safe RAII helper to `server.rs`**

```rust
    /// Xorg `syncEvents.playingEvents` (include/inputstr.h:681-685): true
    /// while `xi1_compute_freezes` is replaying, so a delivery that re-enters
    /// the freeze machinery does not start a nested replay pass.
    pub playing_sync_events: bool,
```

Init `playing_sync_events: false` in `new()`. Add near `Xi1Freeze`:

```rust
/// RAII reset for `ServerState::playing_sync_events`. Clears the flag on EVERY
/// exit path — early `break`/`return` and unwind — so a partial replay never
/// leaves it stuck `true` (which would wedge all future thaws).
///
/// Holds a raw `*mut bool`, NOT a borrow, so the guard does not alias the
/// `&mut ServerState` handed to the fanout during replay. The flag lives in
/// `ServerState`, which is borrowed for the entire `xi1_compute_freezes` call
/// and thus outlives this local guard; the server is single-threaded.
pub(crate) struct SyncReplayGuard(*mut bool);
impl SyncReplayGuard {
    /// Sets `*flag = true` and returns a guard that resets it to `false` on
    /// drop (every exit path incl. unwind). Stores a raw pointer, NOT a borrow,
    /// so it does not alias the `&mut ServerState` the caller hands to the
    /// fanout during replay.
    ///
    /// # Safety
    /// The `bool` behind `flag` MUST outlive the returned guard. In practice
    /// the only caller passes `&mut state.playing_sync_events` and keeps the
    /// guard as a local whose scope ends before the `&mut ServerState` borrow
    /// does, so the field is always live at drop. Single-threaded — no
    /// concurrent access. Do NOT store the guard beyond the state borrow.
    pub(crate) unsafe fn arm(flag: &mut bool) -> Self {
        *flag = true;
        Self(std::ptr::from_mut(flag))
    }
}
impl Drop for SyncReplayGuard {
    fn drop(&mut self) {
        // SAFETY: upheld by the `arm` contract — the flag outlives the guard.
        unsafe { *self.0 = false; }
    }
}
```

> `arm` is `unsafe` because the raw pointer is lifetime-unconstrained; the single documented call site (below) satisfies the contract. `std::ptr::from_mut` is stable. Keep it `pub(crate)`, not `pub` — it is an internal helper, never a public API.

- [x] **Step 2: Insert the guard into the existing `xi1_compute_freezes` (signature unchanged)**

At the top of the current `xi1_compute_freezes(state: &mut ServerState)` (pointer_fanout.rs:1953):

```rust
pub(crate) fn xi1_compute_freezes(state: &mut ServerState) {
    if state.playing_sync_events {
        return; // Xorg dix/events.c:1329-1333 — already replaying.
    }
    // SAFETY: the guard is a local whose scope ends inside this fn, before the
    // `&mut state` borrow ends; `state.playing_sync_events` is thus live at drop.
    let _replay_guard = unsafe { crate::server::SyncReplayGuard::arm(&mut state.playing_sync_events) };
    // ... existing body UNCHANGED (per-device queue replay + tail core drop) ...
    // The guard clears the flag when it drops at end of scope, including on the
    // early `break`s inside the existing while-let loops.
}
```

Do NOT change the body's behavior — the per-device replay and the tail `frozen_pointer_queue.clear()` drop stay exactly as they are. This task only proves the guard resets correctly across the existing early exits.

- [ ] **Step 3: Add a guard-reset regression test**

```rust
#[test]
fn compute_freezes_clears_playing_flag_on_early_exit() {
    let mut state = ServerState::new();
    // Nothing frozen, nothing pending -> the fn takes an early break path.
    crate::core_loop::pointer_fanout::xi1_compute_freezes(&mut state);
    assert!(!state.playing_sync_events, "guard must clear the flag on exit");
    // Re-entrancy: a set flag makes the fn a no-op and must NOT clear it
    // (only the owning guard clears).
    state.playing_sync_events = true;
    crate::core_loop::pointer_fanout::xi1_compute_freezes(&mut state);
    assert!(state.playing_sync_events, "re-entrant call must not clear the owner's flag");
    state.playing_sync_events = false;
}
```

Run: `cargo test -p yserver-core compute_freezes_clears_playing_flag_on_early_exit` -> PASS.

- [ ] **Step 4: Test + lint**

Run: `cargo test -p yserver-core xi_allow_events_async_device_thaws_freeze_and_replays_queue` -> PASS (behavior unchanged).
Run: `cargo test -p yserver-core` (whole crate) -> all non-ignored PASS.
Run: `cargo clippy --all-targets -- -D warnings` -> clean (the guard is used; no unused params).

- [ ] **Step 5: Commit**

```bash
git add crates/yserver-core/src/
git commit -m "refactor(freeze): add playing_sync_events RAII re-entrancy guard to xi1_compute_freezes"
```

---

## Task 3: Introduce `QueuedInputEvent` + global `sync_pending` (dual-write)

Add the tagged-union type and the global queue. Dual-write the pointer gate (legacy + global) so nothing breaks. No replay change yet.

**Files:**
- Modify: `server.rs` (types + field)
- Modify: `pointer_fanout.rs:470-488` (dual-write gate)

- [x] **Step 1: Define the tagged union + queue entry**

```rust
/// A withheld input event awaiting replay. Yserver withholds events in three
/// forms; each replays through its own path, so the queue preserves all three
/// rather than lossily normalizing to host form. Mirrors the already-processed
/// event Xorg copies into `syncEvents.pending` / `sync.event`
/// (dix/events.c:1220-1221, 4435-4447).
#[derive(Debug, Clone)]
pub enum QueuedInputEvent {
    /// Core/XI2 pointer event, host coords (replay via pointer_event_fanout_to_state).
    HostPointer(crate::host_x11::HostPointerEvent),
    /// Core/XI2 key event (replay via key_fanout::deliver_routed_key or full fanout).
    HostKey(crate::host_x11::HostKeyEvent),
    /// XI1 explicit-device-grab event with full routing metadata
    /// (replay via xi1_route_device_event).
    Xi1Routed(Xi1QueuedEvent),
}

/// One entry in the global `syncEvents.pending` port (dix/events.c:661-685).
/// Device-tagged so cross-device order survives (SyncBoth).
#[derive(Debug, Clone)]
pub struct PendingSyncEvent {
    pub device: u16,
    pub event: QueuedInputEvent,
}
```

Add to `ServerState` + init:

```rust
    /// Global replay queue — Xorg `syncEvents.pending`. Withheld events in
    /// arrival order, replayed by `xi1_compute_freezes` once the owning device
    /// thaws. Supersedes the per-device `Xi1Freeze::{queue}` + legacy core
    /// slots after Task 6.
    pub sync_pending: std::collections::VecDeque<PendingSyncEvent>,
```

- [ ] **Step 2: Build + lint**

Run: `cargo build -p yserver-core` then `cargo clippy --all-targets -- -D warnings`
Expected: clean (the new field is used by the test in Step 4; if clippy flags dead code before then, add the Step-4 test in the same commit).

- [x] **Step 3: Dual-write the pointer gate**

At the gate (`pointer_fanout.rs:486`). `HostPointerEvent` is `Copy` (verify: tests use `..press`), so both queues can take it:

```rust
        state.frozen_pointer_queue.push_back(event);
        state.sync_pending.push_back(crate::server::PendingSyncEvent {
            device: crate::xinput::DEVICEID_SLAVE_POINTER,
            event: crate::server::QueuedInputEvent::HostPointer(event),
        });
        return dropped;
```

> If `HostPointerEvent` is not `Copy`, clone into the legacy queue and move into `sync_pending`. Confirm by building.

- [ ] **Step 4: Test the dual-write**

Use the `install_client` pattern (not RecordingBackend delivery). Assert `state.sync_pending` gained a `HostPointer` entry after a frozen-gate pass:

```rust
#[test]
fn frozen_pointer_gate_writes_global_sync_pending() {
    use crate::host_x11::{HostPointerEvent, PointerEventKind};
    use crate::server::{Xi1Freeze, Xi1SyncState, QueuedInputEvent};
    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    let xid_map = backend.xid_map().clone();
    state.xi1_frozen.insert(
        crate::xinput::DEVICEID_SLAVE_POINTER,
        Xi1Freeze { state: Xi1SyncState::FrozenNoEvent, ..Default::default() },
    );
    let ev = HostPointerEvent {
        kind: PointerEventKind::ButtonRelease, host_xid: 0, detail: 1, time: 1,
        root_x: 0, root_y: 0, event_x: 0, event_y: 0, state: 0, crossing_mode: 0, child: 0,
    };
    let _ = crate::core_loop::pointer_fanout::pointer_event_fanout_to_state(
        &mut state, &mut backend, &xid_map, ev, true, false);
    assert_eq!(state.sync_pending.len(), 1);
    assert!(matches!(state.sync_pending[0].event, QueuedInputEvent::HostPointer(_)));
    assert_eq!(state.sync_pending[0].device, crate::xinput::DEVICEID_SLAVE_POINTER);
}
```

Run: `cargo test -p yserver-core frozen_pointer_gate_writes_global_sync_pending`
Expected: PASS. Then `cargo clippy --all-targets -- -D warnings` clean.

- [ ] **Step 5: Commit**

```bash
git add crates/yserver-core/src/
git commit -m "feat(freeze): add QueuedInputEvent + global sync_pending; dual-write pointer gate"
```

---

## Task 4a: Migrate ALL withhold writers to the global queue (blocking prerequisite for delete)

Before any per-device queue is deleted, every writer of `Xi1Freeze.queue`, `core_key_queue`, and the core keyboard slot must dual-write (or single-write) `sync_pending` with the correct `QueuedInputEvent` variant. This is the blocking dependency codex flagged — XI1 explicit device grabs, XI1 fake-motion, and keyboard paths otherwise silently lose events.

**Files:**
- Modify: `pointer_fanout.rs:1632` (`xi1_route_device_event` → also push `Xi1Routed`)
- Modify: `key_fanout.rs:140` (core key withhold → also push `HostKey`)
- Modify: `process_request.rs:36329` (explicit-grab test path — update the test to match the new queue)
- Modify: XI1 activation/replay `pointer_fanout.rs:1700-1795`, `:1764`, `:2060-2065`

- [x] **Step 1: Migrate the XI1 device-event queue writer**

At `pointer_fanout.rs:1632`, where `freeze.queue.push_back(q)` withholds an `Xi1QueuedEvent`, dual-write:

```rust
        freeze.queue.push_back(q.clone());
        state.sync_pending.push_back(crate::server::PendingSyncEvent {
            device: q.deviceid,
            event: crate::server::QueuedInputEvent::Xi1Routed(q),
        });
```

> Watch the borrow: `freeze` is `&mut` into `state.xi1_frozen`; you cannot also `&mut state.sync_pending` while it is live. Restructure: compute `q`, drop the `freeze` borrow, then push both. Or push to `sync_pending` first (state-level), then re-borrow `freeze` for its queue.

- [x] **Step 2: Migrate the core-key withhold writer**

At `key_fanout.rs:140`, where a core key is withheld into `core_key_queue`, dual-write a `HostKey` into `sync_pending` (device = `DEVICEID_SLAVE_KEYBOARD`). Same borrow discipline.

- [ ] **Step 3: Migrate XI1 activating `stored` write**

At `pointer_fanout.rs:1700-1795` where the XI1 activating event is stored (XI form), write it as `QueuedInputEvent::Xi1Routed` into the per-device `stored` (Task 6 retypes `stored`; for now dual-store: keep the existing XI-form `stored` AND record enough to reconstruct — simplest is to defer the `stored` retype entirely to Task 6 and here only ensure the *queue* migration). Scope Task 4a to the QUEUE writers; leave `stored` for Task 6.

- [ ] **Step 4: Update the explicit-grab test**

`process_request.rs:36329` writes `core_key_queue` directly. Update it to also populate `sync_pending` (or, once Task 6 deletes `core_key_queue`, to populate only `sync_pending`). For Task 4a keep it dual-writing so the test still exercises the legacy path.

- [ ] **Step 5: Test + lint**

Run: `cargo test -p yserver-core` (whole crate)
Expected: all non-ignored PASS (dual-write is additive; nothing reads `sync_pending` for replay yet).

Run: `cargo clippy --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add crates/yserver-core/src/
git commit -m "feat(freeze): dual-write all XI1/core withhold paths into global sync_pending"
```

---

## Task 4b: Sole replayer over the global queue (signature change + Xorg loop; fixes Issue 2)

Add `backend`/`xid_map` to `xi1_compute_freezes`/`xi1_thaw_device` (now that they are *used*), update every caller, port Xorg's global restart-from-head loop over `sync_pending` dispatching by variant, and stop `apply_allow_events` hand-draining. This is where the Issue-2 replay behavior lands.

**Files:**
- Modify: `pointer_fanout.rs:1953-1996` (`xi1_compute_freezes`), `:1940` (`xi1_thaw_device`)
- Modify: every `xi1_compute_freezes` / `xi1_thaw_device` caller (grep both — verified list below)
- Modify: `process_request.rs:19960-20114` (`apply_allow_events`)

- [x] **Step 1: Change signatures + update ALL callers (grep exhaustively)**

Add the params and forward them; the guard/re-entrancy check from Task 2 stays:

```rust
pub(crate) fn xi1_compute_freezes(
    state: &mut ServerState,
    backend: &mut dyn crate::backend::Backend,
    xid_map: &HostXidMap,
) {
    if state.playing_sync_events { return; }
    // SAFETY: guard is a local whose scope ends before the `&mut state` borrow.
    let _replay_guard = unsafe { crate::server::SyncReplayGuard::arm(&mut state.playing_sync_events) };
    // body replaced in Step 4
}

pub(crate) fn xi1_thaw_device(
    state: &mut ServerState,
    backend: &mut dyn crate::backend::Backend,
    xid_map: &HostXidMap,
    deviceid: u16,
) {
    if let Some(f) = state.xi1_frozen.get_mut(&deviceid) {
        f.state = crate::server::Xi1SyncState::Thawed;
        f.other = None;
    }
    xi1_compute_freezes(state, backend, xid_map);
}
```

Caller pattern: `let xid_map = backend.xid_map().clone(); xi1_compute_freezes(state, backend, &xid_map);`.
Update EVERY site — grep `xi1_compute_freezes(` and `xi1_thaw_device(`, do not trust a static list. Known sites: `pointer_fanout.rs:2028, :2089, :2115`; `process_request.rs:13759, :13769, :13804, :13821, :13845, :20114`; and the `xi1_thaw_device` callers **including `process_disconnect.rs:386` and `process_request.rs:19270`** (both omitted from earlier hand-lists). Any enclosing fn without `backend` in scope gains a `backend: &mut dyn Backend` param — trace up to the request dispatcher (all request handlers already receive `backend`).

Build after this step: `cargo build -p yserver-core` — resolve every call-site error. Do NOT run tests yet (loop still per-device; behavior is transitional but compiles).

- [ ] **Step 2: Write the cross-device ordering test (real clients, wire-order, ONE compute_freezes call)**

Critical: enqueue both events, set BOTH devices' freeze states to `Thawed`, then call `xi1_compute_freezes` EXACTLY ONCE. Calling `xi1_thaw_device` twice would replay after the first thaw and cannot test cross-device order. One client selects both a key event type and a pointer event type on its window; assert the key event's wire bytes precede the pointer event's.

```rust
/// PlayReleasedEvents replays in GLOBAL arrival order across devices
/// (Xorg dix/events.c:1233-1291), not per-device. A keyboard event queued
/// before a pointer event must replay first once both thaw in one pass.
#[test]
fn compute_freezes_replays_global_queue_in_arrival_order() {
    use crate::{
        host_x11::{HostPointerEvent, HostKeyEvent, PointerEventKind},
        server::{Xi1Freeze, Xi1SyncState, PendingSyncEvent, QueuedInputEvent},
    };
    const CLIENT_ID: u32 = 1;
    const WIN: u32 = 0x0010_0081;
    const HOST_XID: u32 = 0xCAFE_0081;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, CLIENT_ID);
    peer.set_nonblocking(true).expect("nonblocking");
    let mut backend = RecordingBackend::new();

    state.resources.create_window(
        ClientId(CLIENT_ID),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24, window: ResourceId(WIN), parent: ROOT_WINDOW,
            x: 0, y: 0, width: 100, height: 100, border_width: 0, class: 1,
            visual: crate::resources::ROOT_VISUAL, ..Default::default()
        },
    );
    let _ = state.resources.map_window(ResourceId(WIN));
    crate::backend::Backend::register_top_level(&mut backend, None, ResourceId(WIN), HOST_XID)
        .expect("register host xid");
    // Select KeyPress|KeyRelease|ButtonPress|ButtonRelease (0x1|0x2|0x4|0x8 = 0xF).
    state.clients.get_mut(&CLIENT_ID).unwrap()
        .event_masks.insert(ResourceId(WIN), 0x0000_000f);
    // Key delivery needs focus on the window (or an ancestor).
    state.core_focus = crate::server::CoreFocus { raw: WIN, revert_to: 0, time: 0 };

    // Both devices frozen; enqueue KEY (release) THEN POINTER (release).
    for dev in [crate::xinput::DEVICEID_SLAVE_KEYBOARD, crate::xinput::DEVICEID_SLAVE_POINTER] {
        state.xi1_frozen.insert(dev, Xi1Freeze { state: Xi1SyncState::FrozenNoEvent, ..Default::default() });
    }
    // HostKeyEvent (host_x11/pump.rs:277) has NO Default — spell every field.
    // A KeyRelease (pressed:false) of keycode 38 ('a'), routed to core focus.
    state.sync_pending.push_back(PendingSyncEvent {
        device: crate::xinput::DEVICEID_SLAVE_KEYBOARD,
        event: QueuedInputEvent::HostKey(HostKeyEvent {
            pressed: false, keycode: 38, time: 0x1000,
            root_x: 5, root_y: 5, event_x: 5, event_y: 5, state: 0,
        }),
    });
    state.sync_pending.push_back(PendingSyncEvent {
        device: crate::xinput::DEVICEID_SLAVE_POINTER,
        event: QueuedInputEvent::HostPointer(HostPointerEvent {
            kind: PointerEventKind::ButtonRelease, host_xid: HOST_XID, detail: 1, time: 0x2000,
            root_x: 5, root_y: 5, event_x: 5, event_y: 5, state: 0, crossing_mode: 0, child: 0,
        }),
    });

    // Thaw BOTH first, then ONE replay pass.
    for dev in [crate::xinput::DEVICEID_SLAVE_KEYBOARD, crate::xinput::DEVICEID_SLAVE_POINTER] {
        state.xi1_frozen.get_mut(&dev).unwrap().state = Xi1SyncState::Thawed;
    }
    let xid_map = backend.xid_map().clone();
    crate::core_loop::pointer_fanout::xi1_compute_freezes(&mut state, &mut backend, &xid_map);

    // Read the wire; the KeyRelease (event code 3) must precede ButtonRelease (5).
    let mut buf = [0u8; 8192];
    let n = peer.read(&mut buf).expect("read wire");
    assert!(n > 0, "expected replayed events on the wire");
    let key_pos = buf[..n].chunks(32).position(|ev| ev[0] == 3);   // KeyRelease
    let btn_pos = buf[..n].chunks(32).position(|ev| ev[0] == 5);   // ButtonRelease
    assert!(key_pos.is_some() && btn_pos.is_some(), "both events must be delivered");
    assert!(key_pos < btn_pos, "keyboard event (queued first) must replay before pointer");
    assert!(state.sync_pending.is_empty(), "queue fully drained");
}
```

> Confirm `HostKeyEvent`'s field names / `Default` and the 32-byte core-event stride against the wire encoder before running; adjust the chunk stride if events carry a different size. If focus-based key routing needs more setup than `core_focus`, mirror the focus setup in `core_replay_keyboard_releases_grab_and_replays_to_focus` (`process_request.rs:36104`).

Run: `cargo test -p yserver-core compute_freezes_replays_global_queue_in_arrival_order`
Expected: FAIL now (loop still per-device / drops core queue).

- [ ] **Step 3: Author + un-ignore the Issue-2 pin (full body)**

Now that `xi1_thaw_device` replays, write the Issue-2 pin with real asserts (this is its first green stage — deferred from Task 1). Frozen pointer + a withheld release in `sync_pending`; out-of-band thaw; assert delivery.

```rust
/// ISSUE 2 (known-issues.md 2026-07-15): a non-AllowEvents thaw must REPLAY
/// the withheld queue (Xorg ComputeFreezes -> PlayReleasedEvents,
/// dix/events.c:1368-1372), not drop it.
#[test]
fn out_of_band_pointer_thaw_replays_withheld_release() {
    use crate::{
        host_x11::{HostPointerEvent, PointerEventKind},
        server::{Xi1Freeze, Xi1SyncState, PendingSyncEvent, QueuedInputEvent},
    };
    const GRAB_CLIENT_ID: u32 = 1;
    const GRAB_WIN: u32 = 0x0010_0091;
    const HOST_XID: u32 = 0xCAFE_0091;

    let mut state = ServerState::new();
    let mut grab_peer = install_client(&mut state, GRAB_CLIENT_ID);
    grab_peer.set_nonblocking(true).expect("nonblocking");
    let mut backend = RecordingBackend::new();
    state.resources.create_window(
        ClientId(GRAB_CLIENT_ID),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24, window: ResourceId(GRAB_WIN), parent: ROOT_WINDOW,
            x: 0, y: 0, width: 100, height: 100, border_width: 0, class: 1,
            visual: crate::resources::ROOT_VISUAL, ..Default::default()
        },
    );
    let _ = state.resources.map_window(ResourceId(GRAB_WIN));
    crate::backend::Backend::register_top_level(&mut backend, None, ResourceId(GRAB_WIN), HOST_XID)
        .expect("register");
    state.clients.get_mut(&GRAB_CLIENT_ID).unwrap()
        .event_masks.insert(ResourceId(GRAB_WIN), 0x0000_000c); // ButtonPress|ButtonRelease
    state.pointer_grab = Some((ClientId(GRAB_CLIENT_ID), ResourceId(GRAB_WIN)));
    state.pointer_grab_is_passive = true;
    state.xi1_frozen.insert(
        crate::xinput::DEVICEID_SLAVE_POINTER,
        Xi1Freeze { state: Xi1SyncState::FrozenNoEvent, ..Default::default() },
    );
    // A release withheld during the freeze, sitting in the global queue.
    state.sync_pending.push_back(PendingSyncEvent {
        device: crate::xinput::DEVICEID_SLAVE_POINTER,
        event: QueuedInputEvent::HostPointer(HostPointerEvent {
            kind: PointerEventKind::ButtonRelease, host_xid: HOST_XID, detail: 1, time: 0x2a50,
            root_x: 10, root_y: 10, event_x: 10, event_y: 10, state: 0, crossing_mode: 0, child: 0,
        }),
    });

    // OUT-OF-BAND thaw (NOT AllowEvents).
    let xid_map = backend.xid_map().clone();
    crate::core_loop::pointer_fanout::xi1_thaw_device(
        &mut state, &mut backend, &xid_map, crate::xinput::DEVICEID_SLAVE_POINTER);

    let mut buf = [0u8; 4096];
    let n = grab_peer.read(&mut buf).expect("read");
    assert!(buf[..n].chunks(32).any(|ev| ev[0] == 5), "withheld ButtonRelease must be replayed, not dropped");
    assert!(state.sync_pending.is_empty(), "queue drained on thaw");
}
```

Remove any `#[ignore]` (the pin was never committed with one — it is authored here for the first time).

- [x] **Step 4: Replace the loop with the global port**

```rust
    // (inside xi1_compute_freezes, after the guard)
    // Xorg PlayReleasedEvents (dix/events.c:1233-1291): pop the first pending
    // event whose device is unfrozen, deliver through the full path, RESTART
    // from head (a replay may thaw/re-freeze another device).
    loop {
        let idx = state.sync_pending.iter().position(|p| {
            !state.xi1_frozen.get(&p.device).is_some_and(crate::server::Xi1Freeze::frozen)
        });
        let Some(idx) = idx else { break };
        let pending = state.sync_pending.remove(idx).expect("in bounds"); // pop BEFORE deliver
        match pending.event {
            crate::server::QueuedInputEvent::HostPointer(ev) => {
                let _ = pointer_event_fanout_to_state(state, backend, xid_map, ev, false, false);
            }
            crate::server::QueuedInputEvent::HostKey(ev) => {
                let _ = crate::core_loop::key_fanout::deliver_routed_key(state, ev);
            }
            crate::server::QueuedInputEvent::Xi1Routed(q) => {
                let _ = xi1_route_device_event(state, q, true);
            }
        }
    }
    // guard drops here → clears playing_sync_events
```

Delete the old per-device `queue`/`core_key_queue` replay loop and the tail `frozen_pointer_queue.clear()` drop. (Their fields are still present until Task 6, but this fn no longer reads them.)

- [x] **Step 5: Stop hand-draining in `apply_allow_events`**

Delete the non-replay drain (`:20065-20076`) and the queue-drain half of the replay block (`:20098-20106`). Keep the activating-event replay (`:20079-20097`) for now (Task 6 moves it to `stored`). The closing `xi1_compute_freezes` (`:20114`) now owns all queue replay.

> **Double-delivery check:** the legacy `frozen_pointer_queue` is still dual-written (Task 3) but must no longer be *drained* anywhere. Grep every `frozen_pointer_queue` drain/iterate and confirm only the gate write remains. If any drain remains, an event delivers twice.

- [ ] **Step 6: Tests + lint**

Run: `cargo test -p yserver-core compute_freezes_replays_global_queue_in_arrival_order` -> PASS.
Run: `cargo test -p yserver-core out_of_band_pointer_thaw_replays_withheld_release` -> PASS.
Run: `cargo test -p yserver-core xi_allow_events_async_device_thaws_freeze_and_replays_queue` -> PASS (marco/menu order preserved).
Run: `cargo test -p yserver-core` (whole crate) -> all non-ignored PASS; **watch for duplicate-delivery failures**.
Run: `cargo clippy --all-targets -- -D warnings` -> clean.

- [ ] **Step 7: Commit**

```bash
git add crates/yserver-core/src/
git commit -m "fix(freeze): xi1_compute_freezes is the sole replayer over the global queue (known-issues Issue 2)"
```

---

## Task 5: Unified state = sole AllowEvents admission authority (fixes Issue 1)

**Files:**
- Modify: `process_request.rs:19880-19886` (gate); fixtures at `:34112`, `:34401`, `:36104`

- [ ] **Step 1: Un-ignore the Issue-1 pin; confirm it fails**

Remove `#[ignore]`. Run: `cargo test -p yserver-core xi_allow_events_replay_no_op_when_unified_thawed` → FAIL.

- [x] **Step 2: Collapse the gate to Xorg's predicate**

Replace `:19880-19886`:

```rust
    let this_synced = state.xi1_frozen.get(&dev_this).and_then(|f| f.other) == Some(client_id);
    // Xorg AllowSome (dix/events.c:1851): unified per-device state is the SOLE
    // admission authority. The legacy core slot is NOT consulted (it lingered
    // past out-of-band thaws → stale replay; known-issues Issue 1, 2026-07-15).
    if !((this_grabbed && this_state >= Xi1SyncState::FrozenNoEvent) || this_synced) {
        debug!(
            "client {} #{} AllowEvents no-op (grabbed={this_grabbed} state={this_state:?} synced={this_synced})",
            client_id.0, sequence.0
        );
        return Ok(RequestOutcome::Handled);
    }
```

Delete the `core_frozen` binding (`:19880-19884`).

- [ ] **Step 3: Issue-1 pin now green**

Run: `cargo test -p yserver-core xi_allow_events_replay_no_op_when_unified_thawed` → PASS.

- [ ] **Step 4: Confirm the 3 tuned tests break, then fix fixtures**

Run each separately:
```
cargo test -p yserver-core xi_allow_events_replay_device_replays_frozen_button_press_to_target
cargo test -p yserver-core xi_allow_events_replay_device_drains_queued_release_after_press
cargo test -p yserver-core core_replay_keyboard_releases_grab_and_replays_to_focus
```
Expected: FAIL (they set only the legacy slot).

Fix each fixture: add the matching unified `Xi1Freeze` state. Replay (`NOT_GRABBED`) acts only on `FROZEN_WITH_EVENT` (Xorg `:1899`), so use `FrozenWithEvent`; async/sync tests use `FrozenNoEvent`. Example (pointer replay tests):

```rust
    state.xi1_frozen.insert(
        crate::xinput::DEVICEID_SLAVE_POINTER,
        crate::server::Xi1Freeze {
            state: crate::server::Xi1SyncState::FrozenWithEvent,
            ..Default::default()
        },
    );
```

For `core_replay_keyboard_releases_grab_and_replays_to_focus`, key the keyboard device. Verify each test's existing `Xi1Freeze` insert (some already have one at `:34067`) and only add/adjust `state` to match what the test asserts.

- [ ] **Step 5: 3 tuned tests green; whole suite**

Run each of the three separately → PASS.
Run: `cargo test -p yserver-core` → all PASS (the Issue-2 pin was already made green in Task 4b Step 3; nothing here regresses it).
Run: `cargo clippy --all-targets -- -D warnings` → clean.

- [ ] **Step 6: Commit**

```bash
git add crates/yserver-core/src/core_loop/process_request.rs
git commit -m "fix(freeze): unified state is sole AllowEvents admission authority (known-issues Issue 1)"
```

---

## Task 6: Move activating event into per-device `stored`; delete legacy slots + per-device queues (fixes Issue 2)

Retype `stored` to `Option<QueuedInputEvent>`, single-write the gate (pre-map), delete the legacy core slots, `frozen_pointer_queue`, `Xi1Freeze.queue`, and `core_key_queue`. Fix cleanup semantics (not blanket-drop). Complete the Issue-2 pin.

**Files:**
- Modify: `server.rs` (retype `stored`; delete `frozen_pointer_event`, `frozen_pointer_queue`, `frozen_keyboard_event`, `Xi1Freeze.queue`, `core_key_queue`)
- Modify: `pointer_fanout.rs`, `process_request.rs`, `key_fanout.rs`, `process_disconnect.rs`

- [ ] **Step 1: Confirm the Issue-2 pin still passes**

The Issue-2 pin was authored and turned green in Task 4b Step 3 (it drives `sync_pending` directly, which survives this task's field deletions). Re-run it to confirm no regression from the `stored`/gate changes below.

Run: `cargo test -p yserver-core out_of_band_pointer_thaw_replays_withheld_release` → PASS.

- [x] **Step 2: Retype `stored` and migrate its readers**

`server.rs`: `pub stored: Option<crate::server::QueuedInputEvent>,`.

Migrate the XI-form reader at `process_request.rs:13781` (which passes `stored` to `xi1_route_device_event` needing full `Xi1QueuedEvent`): match `Some(QueuedInputEvent::Xi1Routed(q))` and pass `q`; for `HostPointer`/`HostKey` activating events, route through the host replay helper instead. Migrate readers/writers at `pointer_fanout.rs:1764`, `:2060-2065`, and the test at `~:2981`.

> This preserves Xorg's replayDev/replayWin semantics (`:1319-1374`, `:1898-1906`): the activating event replays via its native path (XI1 → `xi1_route_device_event` which honors `focus_route`/`replay_floor`; core → `replay_frozen_pointer_event_to_state`).

- [x] **Step 3: Single-write the gate with pre-button-map detail**

At `pointer_fanout.rs`, capture the physical detail before the button-map block (`:92`), and at the gate store the pre-map event; delete the legacy `frozen_pointer_queue.push_back`:

```rust
    // near :92
    let physical_detail = event.detail;
    // ... button-map block (:92-103) unchanged ...
    // at the gate:
    let mut canonical = event;
    canonical.detail = physical_detail;
    state.sync_pending.push_back(crate::server::PendingSyncEvent {
        device: crate::xinput::DEVICEID_SLAVE_POINTER,
        event: crate::server::QueuedInputEvent::HostPointer(canonical),
    });
    return dropped;
```

State in a comment: a physical→logical map of 0 returns at `:99-101` and never reaches the gate, so no vanished button is ever queued.

- [ ] **Step 4: Write activating core events into `stored` (pre-map), delete legacy writes**

At `pointer_fanout.rs:849` and `server.rs:2653` — carry the physical detail to these sites (they are on the same fanout path; thread `physical_detail` or recompute from the pre-map event) and write:

```rust
    state.xi1_frozen.entry(crate::xinput::DEVICEID_SLAVE_POINTER).or_default().stored =
        Some(crate::server::QueuedInputEvent::HostPointer(pre_map_event));
```

Keyboard activating event likewise into the keyboard device `stored` as `QueuedInputEvent::HostKey`.

- [ ] **Step 5: Read `stored` for replay in `apply_allow_events`**

Replace the `frozen_pointer_event.take()` / `frozen_keyboard_event.take()` reads (`:19971`, `:20013`) with taking the device `stored` on the Replay path and dispatching by variant to the correct replay helper. Delete the `std::mem::take(&mut state.frozen_pointer_queue)` (`:19976`) and all legacy-queue handling.

- [x] **Step 6: Delete the fields; fix cleanup semantics per site**

Delete `frozen_pointer_event`, `frozen_pointer_queue`, `frozen_keyboard_event`, `Xi1Freeze.queue`, `core_key_queue` + inits. Compile; fix each error. **Cleanup is per-site, not blanket** (codex #7):

- **Device destroy** (input device gone): discard that device's pending — `state.sync_pending.retain(|p| p.device != dev)` — AND clear its `stored`.
- **Grab deactivate / passive-grab release after physical button-up** (`server.rs:2623`, the passive-release path): do NOT discard unrelated pending. Set the device `Thawed` and call `xi1_compute_freezes` so surviving pending REPLAYS (Xorg `:1233-1291`). Clear only this grab's `stored`.
- **Client disconnect** (`process_disconnect.rs:335`, `:386`): `PendingSyncEvent { device, event }` carries NO delivery target, so per-target filtering is not possible and must NOT be attempted. Instead follow Xorg: for each device this client grabbed, thaw + `xi1_compute_freezes` so surviving pending REPLAYS (Xorg `CloseDownClient` → `UngrabAllDevices` deactivates the grab → `ComputeFreezes`/`PlayReleasedEvents`, `dix/events.c:1233-1291`). Events destined for the disconnecting client's now-destroyed windows no-op harmlessly during replay (the resource is gone), so no target filtering is needed. Never blanket-drop `sync_pending` on disconnect — that reintroduces Issue 2 via the ungrab path. (If, and only if, a future need to filter by target arises, add a `target: ResourceId` to `PendingSyncEvent` — do not fake it here.)
- **Core-bridge release** (`process_request.rs:11660`, `:11815`, `:24047`): clear this device's `stored`; thaw + replay if the bridge release implies a thaw.

Audit each of the ~6 clear sites individually; annotate which category it is.

- [ ] **Step 7: Add the non-identity button-map regression test**

Set `state.pointer_mapping_override = Some(vec![3,2,1,...])` (physical 1→logical 3), freeze the pointer, queue a physical button-1 press+release through the gate, thaw, and assert the replayed event delivers logical button **3** exactly once (not 3-then-remapped-again). Real-client wire assertion.

Run: `cargo test -p yserver-core <that test>` → PASS.

- [ ] **Step 8: Whole suite + lint (all pins green, no ignores)**

Run: `cargo test -p yserver-core` → ALL PASS; grep the test file for `#[ignore` and confirm neither conformance pin (Issue 1, Issue 2) remains ignored.
Run: `cargo clippy --all-targets -- -D warnings` → clean.

- [ ] **Step 9: Commit**

```bash
git add crates/yserver-core/
git commit -m "fix(freeze): single freeze record (stored + global sync_pending); delete legacy slots + per-device queues (known-issues Issue 2)"
```

---

## Task 7: Conformance pass, docs, hardware smoke

**Files:**
- Modify: `docs/known-issues.md`

- [ ] **Step 1: Toolchain gates (exact CI commands)**

Run: `cargo +nightly fmt`
Run: `cargo clippy --all-targets -- -D warnings`
Run: `cargo test -p yserver-core`
Expected: fmt clean, clippy clean, all tests pass.

- [ ] **Step 2: XTS grab/AllowEvents subset**

Run the AllowEvents / grab XTS cases on the HW xts harness (bee tmux — see memory `reference_hw_xts_via_tmux.md`, `reference_xts.md`) and diff against the measured Xorg profile (Xorg itself fails ~47/316 XI — `reference_xorg_not_100pct_on_xts_xi.md`; compare to profile, not 100%).
Expected: no regression vs the pre-change profile on grab/AllowEvents cases.

- [ ] **Step 3: Hardware smoke — the two repro paths (per `feedback_no_commit_before_smoke.md`)**

Unit tests do not cover the visual/input paths; smoke on real HW:
- Steam (the #94 wedge origin): open Library / a menu — no input wedge.
- Sync-passive-grab WM drag (marco/muffin titlebar drag, Cinnamon rubber-band): gestures complete, no stuck button, no lost release.

Expected: interactive; no stuck grab, no phantom double event.

- [ ] **Step 4: Flip the known-issues entries (mechanics only)**

In `docs/known-issues.md`, change both 2026-07-15 items `- [ ]` → `- [x]` and append the resolving commit hash. The user writes the prose (`feedback_user_writes_docs_prose.md`) — offer the mechanical diff, let the user word it.

- [ ] **Step 5: Final commit**

```bash
git add docs/known-issues.md
git commit -m "docs(known-issues): resolve 2 grab-freeze conformance gaps via freeze-state unification"
```

---

## Self-Review Notes (for the executor)

- **Spec coverage:** Task 5 → Issue 1; Task 4b → Issue 2 (replay on thaw), reconfirmed in Task 6. Tasks 2/3/4a/4b are the enabling refactor codex required (guard, tagged-union queue, migrate writers, sole replayer). Task 1 pins Issue 1; Task 4b authors the Issue-2 pin. Task 7 gates + HW + docs.
- **Type consistency:** `QueuedInputEvent` (3 variants incl. `Xi1Routed(Xi1QueuedEvent)`) + `PendingSyncEvent` defined Task 3 Step 1; `Xi1Freeze::stored` retyped to `Option<QueuedInputEvent>` Task 6 Step 2; the `playing_sync_events` guard is added in Task 2 (signature unchanged); `xi1_compute_freezes(state, backend, xid_map)` / `xi1_thaw_device(state, backend, xid_map, deviceid)` signatures are set in Task 4b Step 1 (deferred from Task 2 so the params are never unused).
- **Blocking dependency:** Task 4a (migrate ALL withhold writers) MUST complete before Task 4b/Task 6 delete the per-device queues, or XI1 device-grab / fake-motion / keyboard paths silently lose events.
- **Double-delivery guard:** legacy queues are dual-written from Task 3/4a but must never be *drained* after Task 4b — grep every drain site.
- **Cleanup categories (Task 6 Step 6):** device-destroy = discard; grab-deactivate / disconnect = thaw-and-replay, never blanket-drop (else Issue 2 returns via ungrab).
- **No committed `todo!()`:** Task 1 and Task 4b tests are full fixtures before their commit.
- **Every commit passes `cargo clippy --all-targets -- -D warnings`** — no "warnings acceptable" intermediate.
- **Open verification for the executor:** exact behavior of `xi1_route_device_event` when replaying `Xi1Routed` from the global queue vs the old per-device path (focus_route/replay_floor honored identically); confirm with the existing XI1 device-grab tests.
