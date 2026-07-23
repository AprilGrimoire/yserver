# XI2/Core Implicit Pointer Grab Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

> **Revision v4 (2026-07-15), full rewrite.** v1 (single-owner) → v2 (multi-client, WRONG — based on an inaccurate codex claim about `DeliverGrabbedEvent`) → v3 (correction banner: single-owner re-verified against source) → **v4 (this)**: v3 re-grounded line-by-line against `../xserver` AND the current yserver code. Two v3 notes were themselves wrong and are corrected here: (a) there is **no button-count transition gate** in Xorg — install is `if (deliveries) { if (!grab && ActivateImplicitGrab(...)) }` (`dix/events.c:2415-2421`) and the 0→nonzero gate would break the XIReplayDevice crux (the replayed press re-runs the `buttons_down` update, so `prior != 0`); teardown is `!b->buttonsDown` *after* the release (`Xi/exevents.c:1935`), not a transition. (b) The routing integration is not a new accessor: reusing `pointer_grab`/`active_pointer_grab` makes every existing consumer (redirects, passive-activation gate, barriers, request handlers, disconnect) behave Xorg-correctly with zero changes — verified handler-by-handler below.

**Goal:** Give yserver the X11 implicit pointer grab: a delivered `ButtonPress` activates a real (async, auto-released) pointer grab owned by the press recipient on the press's event window, so subsequent motion and the matching `ButtonRelease` follow the press instead of being re-hit-tested against a window tree the WM has since mutated. Fixes #94 Cinnamon (muffin's click-to-focus mutates the tree between press and release; Steam saw 164 XI2 presses vs 28 releases → stuck button → clicks dead ~19/20). Lands on branch `codex/freeze-state-unification`; nothing merges until Cinnamon works on HW.

**Architecture:** The implicit grab **is** a real active pointer grab (Xorg `ActivateImplicitGrab` builds a `GrabRec` with `resource = client->clientAsMask`, `window = pWin`, async modes, `implicit=TRUE`, and runs it through the one shared `ActivatePointerGrab`). yserver mirrors that: reuse `ServerState.pointer_grab` + `active_pointer_grab` with a new `implicit: bool` flag on `ActivePointerGrab`. Delivery facts (first successful natural press recipient + event window + protocol) are captured during the fanout; install/teardown run in the two public fanout wrappers **after** delivery completes (Xorg delivers the final release under the grab, then deactivates). One latent routing bug must be fixed for the fast-click case: the active-grab redirect is currently gated on `handle_grabs`, so queued-while-frozen events replayed by `xi1_compute_freezes` (`handle_grabs=false`) bypass the grab — they must honor it (Xorg `PlayReleasedEvents` delivers through `DeliverGrabbedEvent`).

**Tech Stack:** Rust (`crates/yserver-core`). In-crate unit tests: `ServerState::new()` + `install_client` (peer `UnixStream`s) + `read_all_available` + `RecordingBackend` + hand-built `HostXidMap`. Xorg ref: `../xserver/dix/events.c`, `../xserver/Xi/exevents.c` (absolute: `/home/jos/Projects/xserver`).

**Toolchain (AGENTS.md, before EVERY commit):** `cargo +nightly fmt`; `cargo clippy --all-targets -- -D warnings` (regular clippy, exactly as CI); `cargo test -p yserver-core`.

---

## Verified Xorg anchors (all re-read 2026-07-15; do not trust memory, re-open if in doubt)

- `dix/events.c:2150-2193` `ActivateImplicitGrab`: single `GrabRec` — `resource = client->clientAsMask` (ONE owner client), `window = pWin` (the event window the press was delivered on), `ownerEvents = (deliveryMask & OwnerGrabButtonMask)`, `eventMask = deliveryMask`, keyboard/pointer modes both `GrabModeAsync`, `grabtype` = CORE / XI / XI2 by the delivered press form, window's XI+XI2 masks merged in as the grab's device masks. Activated via `ActivateGrab(dev, tempGrab, currentTime, TRUE | ImplicitGrabMask)`.
- `:2415-2429` install site, inside per-window delivery: `if (deliveries) { if (!grab && ActivateImplicitGrab(...)) ... }` — **any** successfully delivered press with no grab in effect; **no button-count / transition condition**. Comment: "since core events are delivered first, an implicit grab may be activated on a core grab, stopping the XI events" → core-first, first successful protocol wins.
- `:1612-1651` `ActivatePointerGrab` (shared by explicit/passive/implicit): sets `grabinfo->grabTime = time` — **implicit activation updates the device grab time**; `fromPassiveGrab = TRUE` and `implicitGrab = TRUE` for the implicit case.
- `:4361-4404` `DeliverGrabbedEvent`: `ownerEvents=true` re-enters `DeliverDeviceEvents` **filtered by the grab** (single client); else `DeliverOneGrabbedEvent` to the one owner. Never a fan-out to all selectors (the v2 error).
- `Xi/exevents.c:1931-1938` final release: `if (grab && !b->buttonsDown && fromPassiveGrab && GrabIsPointerGrab) deactivateDeviceGrab = TRUE` — since implicit grabs have `fromPassiveGrab=TRUE`, this **shared** path deactivates them; `:1946-1958` the release is delivered **under** the grab first, deactivation after.
- `:5150-5158` `ProcUngrabPointer`: `SameClient(grab, client)` → deactivate — a client CAN ungrab its own implicit grab.
- `:5240-5243` `GrabDevice`: `grab && !SameClient` → `AlreadyGrabbed` — another client's `GrabPointer` during an implicit grab fails; same client replaces it.
- `:5113-5131` `ProcChangeActivePointerGrab`: `SameClient` → updates cursor + eventMask; applies to implicit grabs too.
- `:2885-2930` `DeliverDeviceEvents` stops at the first successfully-delivering window — the SEPARATE leaf/ancestor XI2 double-delivery gap (Task 8 decision, not subsumed).

## Verified yserver anchors (current tree, branch `codex/freeze-state-unification` @ f3198c08)

`crates/yserver-core/src/core_loop/pointer_fanout.rs`:
- `:45-62` public `pointer_event_fanout_to_state` → `:80` `pointer_event_fanout_to_state_inner` (only two callers of inner: `:53` and `:76`).
- `:70-77` `replay_frozen_pointer_event_to_state` — the XIReplayDevice re-entry (`handle_grabs=false, is_replay=false, suppress_raw=true`).
- `:111-122` `buttons_down` bit update (runs even for events later queued — why transition gating is unusable).
- `:466-497` queue-while-frozen gate: event **queued and returned before any delivery** (`!is_replay && handle_grabs && frozen`).
- `:596-714` Step-2 core active-grab redirect, gated `handle_grabs &&` at `:597`; mask-gated delivery `:684`; `handled_core_via_grab` capture `:702`.
- `:721` `release_passive_grab_on_button_release` (passive-only: keys on `pointer_grab_is_passive`).
- `:766-768` passive activation gate `!active_grab_present` — with the implicit grab in `pointer_grab`, a second press correctly skips passive matching (Xorg `:1925` `if (!grab && CheckDeviceGrabs...)`).
- `:887-1008` Step-4 natural core propagation (`core_targets` fanout at `:992`).
- `:1016-1018` `is_replay` early return (before XI1/XI2анout); `:1074-1081` `top_level_id_opt` early return (before XI2).
- `:1148-1159` XI2 sync-passive freeze filter; `:1171-1228` XI2 active-grab redirect, gated `handle_grabs &&` at `:1171`; via_xi2 push at `:1200-1205`.
- `:1364-1497` per-recipient XI2 delivery loop (`ev_win` resolution at `:1371-1403`); `:1499` function tail.
- `:1974-2011` `xi1_compute_freezes` drains `sync_pending` via `pointer_event_fanout_to_state(..., false, false)` — the fast-click release path.
- `:2211-2258` `active_grab_target` — non-passive branch reads `active_pointer_grab` (owner_events/via_xi2/event_mask).
- `:3018` test `install_client`, `:3045` `read_all_available`, `:3061` `motion_event`.

`crates/yserver-core/src/server.rs`:
- `:513-535` `ActivePointerGrab` (Copy struct: owner, grab_window, event_mask u16, cursor, time, owner_events, via_xi2).
- `:803` `pointer_grab`, `:813` `active_pointer_grab`, `:816` `pointer_grab_is_passive`, `:835` `last_pointer_grab_time`.
- `:1835` `ClientState.xi2_masks: HashMap<(ResourceId, u16), u32>` (bit = evtype).

`crates/yserver-core/src/core_loop/process_request.rs`:
- `:23922-23933` GrabPointer `grabbed_by_other` → AlreadyGrabbed (owner-keyed — implicit grab behaves Xorg-correctly, no change).
- `:24037-24048` UngrabPointer `held_by_client` → `deactivate_core_pointer_grab` (same-client only — Xorg-correct for implicit, no change).
- `:11589-11624` XIGrabDevice constructor site; `:11822-11846` XIUngrabDevice owner-keyed clear.
- `:19907-19928` AllowEvents authorization: implicit owner passes `this_grabbed` but `this_state` is Thawed (implicit grabs never freeze) → no-op, Xorg-correct.
- `:20021-20043` ReplayPointer/ReplayDevice clears the (passive or explicit sync) grab **before** `:20125` `replay_frozen_pointer_event_to_state` → at replay time `pointer_grab` is None → the install fires. **The #94 crux.**
- `:34141-34278` test `xi_allow_events_replay_device_replays_frozen_button_press_to_target` — the wire-idiom model for ReplayDevice tests; its `:34255` `assert!(state.pointer_grab.is_none())` **must be updated** by this work (the implicit grab now correctly installs on the replayed press).

`process_disconnect.rs:330-351` owner-keyed grab cleanup — covers an implicit owner disconnecting mid-click, no change.

## Design decisions (source-verified)

1. **Reuse the active-grab slot, add `implicit: bool`.** Xorg has ONE `deviceGrab.grab`; every yserver consumer of `pointer_grab`/`active_pointer_grab` was audited and behaves Xorg-correctly with an implicit grab in the slot (anchors above): GrabPointer→AlreadyGrabbed for others / replace for owner; UngrabPointer/XIUngrabDevice→same-client deactivate; ChangeActivePointerGrab→same-client update; AllowEvents→no-op (never frozen); passive-activation gate→skipped during the grab; `xi1_device_grab_owner`/barriers/disconnect→owner-keyed. **No new accessor; the only handler change is the XI1 `ReplayThisDevice` core-bridged TODO (Task 4 Step 2).** Known pre-existing divergence, explicitly OUT OF SCOPE (codex review 2026-07-15 finding 1): `XIGrabDevice` always succeeds and unconditionally overwrites the slot (`process_request.rs:11556-11624` — deliberate permissiveness, "keeps GTK happy"); Xorg would return `AlreadyGrabbed` for another client's grab (`events.c:5240`). An implicit grab can therefore be clobbered by another client's XIGrabDevice exactly as an explicit grab can be today; fixing that is a separate conformance task, not this branch.
2. **Install at fanout completion, not mid-fanout.** Xorg activates the grab inside per-window delivery, which then reroutes the *same press's* remaining (XI) forms. yserver's core/XI2 dedup delivers each press form in one pass; installing mid-pass would reroute this press's own XI2 form through the new grab and change established same-press delivery. Deviation: capture the first successful natural delivery (core first, matching Xorg's protocol order), install after the fanout returns. Only *subsequent* events see the grab. Delivery of the activating press is byte-identical to today.
3. **No transition gates.** Install: press delivered + `pointer_grab.is_none()` + no XI1 device grab on the slave pointer. Teardown: release processed + `buttons_down == 0` + `implicit`. Matches `dix/events.c:2415` / `Xi/exevents.c:1935`. The queue-while-frozen gate returns before delivery → a `queued` flag skips lifecycle for withheld events (their lifecycle runs when the queue replays them).
4. **Redirects follow grab state, not `handle_grabs`.** Drop `handle_grabs &&` from the Step-2 core redirect (`:597`) and the XI2 redirect (`:1171`). With no grab, `active_grab_target` is None → no-op for the ReplayDevice/ReplayPointer press replays (grab already cleared). With a grab, queued events drained by `xi1_compute_freezes` now route under it — which `apply_allow_events`'s own comment (`:20001-20004`, "with the grab still active they route to the grab owner — Xorg PlayReleasedEvents") already claims but the gate silently broke. Required for the fast-click case (release arrives during muffin's freeze, queued, drained after ReplayDevice). `handle_grabs` keeps gating passive-grab *matching* (Step 3) and the freeze *queue/filter* — stage-gates Xorg also has.
5. **Grab parameters from the recipient's selection** (Xorg `deliveryMask`): core → `event_mask = client's window mask & 0xFFFF`, `owner_events = mask & OwnerGrabButton (1<<24)`; XI2 → `via_xi2=true`, `owner_events=false` (XI2 has no OwnerGrabButton; bit 24 of an XI2 mask is a raw-touch bit — matching it would be accidental), `event_mask=0` (unused on the via_xi2 path). `cursor=0` (no sprite override), `time = event.time`, and **`last_pointer_grab_time = event.time`** (Xorg `grabTime`, `events.c:1637` — makes GrabPointer/UngrabPointer/AllowEvents timestamp validation see the click).
6. **XI2 delivery under an implicit grab is mask-gated by a snapshot taken at activation** (Xorg `ActivateImplicitGrab` merges the window's xi2mask — ALL clients' selections on the window — into the grab, `events.c:2183-2189`, and grab delivery filters by it). `ActivePointerGrab` gains `xi2_mask: u32` (bit = evtype): implicit install snapshots the merged mask of every client's `xi2_masks` on the event window; explicit `XIGrabDevice` sets `u32::MAX` (its wire mask is not parsed — today's permissive delivery, pre-existing, out of scope); core `GrabPointer` sets 0 (unused — the XI2 redirect never delivers for `via_xi2=false` grabs). The redirect then gates on `grab.xi2_mask & (1 << evtype)`. (A live lookup was rejected in codex review — Xorg's mask is activation-time state, and the merged-window snapshot also reproduces Xorg's quirk that the owner receives evtypes *another* client selected on the window.)
7. **Bare-clear teardown.** The implicit grab set no cursor override, no confine, no freeze, no crossing chain — so teardown must NOT call `deactivate_core_pointer_grab` (NotifyUngrab chain + freeze-bridge release don't apply and yserver already delivered natural crossings during the grab; emitting the chain would double-deliver). `UngrabPointer` of one's own implicit grab DOES go through the full helper — its extra side effects are no-ops there (nothing frozen, no cursor) and Xorg shares `DeactivatePointerGrab` anyway.
8. **Non-goals** (document, don't build): XI1-typed implicit grabs (yserver's XI1 grab machinery is separate; XTS profile unchanged); NotifyGrab/NotifyUngrab crossing chains on implicit activate/deactivate (Xorg emits them only when the sprite window differs from the grab window — for an implicit grab installed on the press hit they are no-ops in the common case, and yserver delivers natural crossings during grabs, a pre-existing divergence); keyboard (X has no implicit keyboard grab); the XI2 leaf/ancestor double-delivery (separate bug, Task 8 decision).

## Known suite fallout (triage map — failures here are EXPECTED, fix the test)

- `process_request.rs:34255` (`xi_allow_events_replay_device_replays_frozen_button_press_to_target`): `assert!(state.pointer_grab.is_none())` after ReplayDevice → the replayed press now installs the implicit grab. Update to assert `pointer_grab == Some((TARGET_CLIENT, TARGET_WIN))` and `active_pointer_grab.is_some_and(|g| g.implicit && !g.via_xi2)` (target selected core-only in that test). This assertion flip IS the #94 fix pinned.
- Any test that presses on one window and then expects motion/release to deliver naturally elsewhere while a button is logically held will now see grab-redirected routing. For each failure ask "what would Xorg do with the implicit grab active?" — if redirect is the Xorg behavior, update the test (add a release, or assert the redirect); only if the test exposes a real divergence change the code. Do NOT weaken the install/teardown gates to make a test pass; per `feedback_no_confabulation`, run `timeout 600 cargo test -p yserver-core` and read actual failures.

---

## File Structure

- `crates/yserver-core/src/server.rs` — `ActivePointerGrab` gains `implicit: bool` (stays `Copy`).
- `crates/yserver-core/src/core_loop/pointer_fanout.rs` — `ImplicitGrabFanoutInfo` (capture struct), two capture points in the inner fanout, `implicit_pointer_grab_lifecycle` called from both public wrappers, the two redirect-gate fixes, the XI2 implicit mask gate, and all new tests.
- `crates/yserver-core/src/core_loop/process_request.rs` — constructor-site updates (`implicit: false`) + the `:34255` assertion flip + Task 7 acceptance tests (wire-driven XIAllowEvents).
- No other files change. (`process_disconnect.rs`, barriers, XI1 grab owner: audited, correct via reuse.)

---

## Task 1: `implicit` + `xi2_mask` fields on `ActivePointerGrab` (mechanical, no behavior)

**Files:** Modify `crates/yserver-core/src/server.rs:513-535`; all `ActivePointerGrab {` constructor sites.

- [ ] **Step 1: Add the fields** after `via_xi2` in the struct (`server.rs`):

```rust
    /// True when this grab was established implicitly by a delivered
    /// ButtonPress (Xorg `ActivateImplicitGrab`, dix/events.c:2150-2193:
    /// `grabinfo->implicitGrab`), rather than by GrabPointer/XIGrabDevice
    /// or a passive button grab. An implicit grab is a REAL active grab —
    /// request handlers (GrabPointer AlreadyGrabbed, UngrabPointer,
    /// ChangeActivePointerGrab, AllowEvents, disconnect) treat it exactly
    /// like an owned grab, which is Xorg's shared-`deviceGrab.grab` model.
    /// Only the auto-release path keys on it: the pointer fanout tears it
    /// down after the final ButtonRelease is delivered under it
    /// (Xi/exevents.c:1931-1958).
    pub implicit: bool,
    /// XI2 evtype mask (bit = evtype) gating XI2 delivery under this grab
    /// — Xorg `GrabRec.xi2mask`. Snapshot semantics: for an implicit grab
    /// this is the event window's MERGED xi2 selection captured at
    /// activation (Xorg ActivateImplicitGrab: xi2mask_merge(tempGrab->
    /// xi2mask, inputMasks->xi2mask), events.c:2183-2189). XIGrabDevice
    /// sets `u32::MAX` — its wire mask is not parsed (pre-existing
    /// permissive delivery); core GrabPointer sets 0 (never consulted:
    /// the XI2 redirect delivers nothing for via_xi2=false grabs).
    pub xi2_mask: u32,
```

- [ ] **Step 2: Update every constructor site.** Find them all:

Run: `grep -rn "ActivePointerGrab {" crates/yserver-core/src`
Expected sites (verify the grep, don't trust this list blindly): `pointer_fanout.rs:3150, 4537, 4667` (tests), `process_request.rs:11616` (XIGrabDevice), `:23950` (GrabPointer), `:34353, 35488, 36058, 36175` (tests), plus the struct definition. Add `implicit: false,` to each, plus `xi2_mask: u32::MAX,` where `via_xi2: true` (XIGrabDevice and via_xi2 test grabs — preserves today's permissive XI2 grab delivery) and `xi2_mask: 0,` where `via_xi2: false`.

- [ ] **Step 3: Gates + commit**

Run: `cargo +nightly fmt && cargo clippy --all-targets -- -D warnings && cargo test -p yserver-core`
Expected: all green (pure additive field).
```bash
git add -A crates/yserver-core
git commit -m "refactor(#94): ActivePointerGrab::implicit flag (no behavior change)"
```

---

## Task 2: Failing tests — release follows the press recipient (core + XI2)

**Files:** Tests in `crates/yserver-core/src/core_loop/pointer_fanout.rs` `mod tests`.

- [ ] **Step 1: Core failing test.** Uses the module's existing `install_client` / `read_all_available` / `motion_event` helpers. Core events are 32 bytes; ButtonRelease type 5; event window at bytes 12..16.

```rust
    /// X11 implicit pointer grab (#94, Xorg dix/events.c:2150-2193 +
    /// 2415-2421): a delivered ButtonPress activates an async grab owned
    /// by the press recipient on the press's event window; the matching
    /// ButtonRelease is delivered under that grab even when the hit-test
    /// now resolves to another client's window (muffin mutates the tree
    /// between press and release — Steam got presses without releases).
    #[test]
    fn implicit_grab_core_release_follows_press_recipient() {
        use yserver_protocol::x11::{CreateWindowRequest, ResourceId};
        const APP: u32 = 1;
        const OTHER: u32 = 2;
        let win_a = ResourceId(0x0010_0001);
        let win_b = ResourceId(0x0020_0001);
        const HOST_A: u32 = 0xCAFE_0001;
        const HOST_B: u32 = 0xCAFE_0002;

        let mut state = ServerState::new();
        let mut backend = RecordingBackend::default();
        let mut app_peer = install_client(&mut state, APP);
        let mut other_peer = install_client(&mut state, OTHER);

        for (client, win, x) in [(APP, win_a, 0i16), (OTHER, win_b, 500i16)] {
            state.resources.create_window(
                ClientId(client),
                CreateWindowRequest {
                    depth: 24,
                    window: win,
                    parent: ROOT_WINDOW,
                    x,
                    y: 0,
                    width: 100,
                    height: 100,
                    border_width: 0,
                    class: 1,
                    visual: crate::resources::ROOT_VISUAL,
                    ..Default::default()
                },
            );
            let _ = state.resources.map_window(win);
        }
        // ButtonPress|ButtonRelease.
        for (client, win) in [(APP, win_a), (OTHER, win_b)] {
            state
                .clients
                .get_mut(&client)
                .unwrap()
                .event_masks
                .insert(win, 0x0000_000c);
        }
        let mut xid_map = HostXidMap::new();
        xid_map.insert(HOST_A, win_a);
        xid_map.insert(HOST_B, win_b);

        let mut press = motion_event();
        press.kind = PointerEventKind::ButtonPress;
        press.host_xid = HOST_A;
        press.detail = 1;
        press.time = 1000;
        let _ =
            pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, press, true, false);
        assert!(
            state
                .active_pointer_grab
                .is_some_and(|g| g.implicit && g.owner == ClientId(APP) && g.grab_window == win_a),
            "delivered press must install the implicit grab (Xorg dix/events.c:2415)"
        );
        let _ = read_all_available(&mut app_peer); // drain the press

        // Release resolves over OTHER's window (the WM-mutated-tree shape).
        let mut release = motion_event();
        release.kind = PointerEventKind::ButtonRelease;
        release.host_xid = HOST_B;
        release.detail = 1;
        release.time = 1010;
        release.root_x = 550;
        release.root_y = 10;
        release.event_x = 50;
        release.event_y = 10;
        release.state = 0x100;
        let _ =
            pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, release, true, false);

        let bytes = read_all_available(&mut app_peer);
        let mut saw_release = false;
        let mut off = 0usize;
        while off + 32 <= bytes.len() {
            if bytes[off] & 0x7F == 5 {
                saw_release = true;
                assert_eq!(
                    &bytes[off + 12..off + 16],
                    &win_a.0.to_le_bytes(),
                    "grabbed release must be reported on the grab (press) window"
                );
            }
            off += 32;
        }
        assert!(
            saw_release,
            "implicit grab: the release must follow the press recipient, not re-hit-test"
        );
        let other_bytes = read_all_available(&mut other_peer);
        assert!(
            !other_bytes.chunks(32).any(|c| c[0] & 0x7F == 5),
            "the grab captures the release — OTHER must not receive it"
        );
        assert!(
            state.pointer_grab.is_none() && state.active_pointer_grab.is_none(),
            "final release tears the implicit grab down (Xi/exevents.c:1931)"
        );
    }
```

- [ ] **Step 2: XI2 failing test.** XI2 XGE: first byte 35, evtype u16 at offset 8, event window u32 at offset 24, advance `32 + length_at_offset_4 * 4` (model: `xi2_grabbed_button_release_reaches_grab_owner_via_grab_window`, pointer_fanout.rs:3174-3196).

```rust
    /// XI2 form of the implicit grab (#94 — the actual Steam/Cinnamon
    /// shape: Steam selects cooked XI2 buttons, no core mask). The XI2
    /// ButtonPress installs a via_xi2 implicit grab; the release delivers
    /// to the owner on the grab window through the XI2 redirect.
    #[test]
    fn implicit_grab_xi2_release_follows_press_recipient() {
        use yserver_protocol::x11::{CreateWindowRequest, ResourceId};
        const APP: u32 = 1;
        const OTHER: u32 = 2;
        let win_a = ResourceId(0x0010_0001);
        let win_b = ResourceId(0x0020_0001);
        const HOST_A: u32 = 0xCAFE_0001;
        const HOST_B: u32 = 0xCAFE_0002;

        let mut state = ServerState::new();
        let mut backend = RecordingBackend::default();
        let mut app_peer = install_client(&mut state, APP);
        let _other_peer = install_client(&mut state, OTHER);

        for (client, win, x) in [(APP, win_a, 0i16), (OTHER, win_b, 500i16)] {
            state.resources.create_window(
                ClientId(client),
                CreateWindowRequest {
                    depth: 24,
                    window: win,
                    parent: ROOT_WINDOW,
                    x,
                    y: 0,
                    width: 100,
                    height: 100,
                    border_width: 0,
                    class: 1,
                    visual: crate::resources::ROOT_VISUAL,
                    ..Default::default()
                },
            );
            let _ = state.resources.map_window(win);
        }
        // XI_ButtonPress(4) | XI_ButtonRelease(5) on the master pointer.
        state
            .clients
            .get_mut(&APP)
            .unwrap()
            .xi2_masks
            .insert((win_a, XI2_MASTER_POINTER_DEVICE_ID), (1 << 4) | (1 << 5));
        state
            .clients
            .get_mut(&OTHER)
            .unwrap()
            .xi2_masks
            .insert((win_b, XI2_MASTER_POINTER_DEVICE_ID), (1 << 4) | (1 << 5));
        let mut xid_map = HostXidMap::new();
        xid_map.insert(HOST_A, win_a);
        xid_map.insert(HOST_B, win_b);

        let mut press = motion_event();
        press.kind = PointerEventKind::ButtonPress;
        press.host_xid = HOST_A;
        press.detail = 1;
        press.time = 1000;
        let _ =
            pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, press, true, false);
        assert!(
            state
                .active_pointer_grab
                .is_some_and(|g| g.implicit && g.via_xi2 && g.owner == ClientId(APP)),
            "XI2-delivered press must install a via_xi2 implicit grab"
        );
        let _ = read_all_available(&mut app_peer);

        let mut release = motion_event();
        release.kind = PointerEventKind::ButtonRelease;
        release.host_xid = HOST_B;
        release.detail = 1;
        release.time = 1010;
        release.root_x = 550;
        release.root_y = 10;
        release.event_x = 50;
        release.event_y = 10;
        release.state = 0x100;
        let _ =
            pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, release, true, false);

        let bytes = read_all_available(&mut app_peer);
        let mut found_win = None;
        let mut off = 0usize;
        while off + 32 <= bytes.len() {
            if bytes[off] == 35 && u16::from_le_bytes([bytes[off + 8], bytes[off + 9]]) == 5 {
                found_win = Some(u32::from_le_bytes(
                    bytes[off + 24..off + 28].try_into().unwrap(),
                ));
                break;
            }
            let length =
                u32::from_le_bytes(bytes[off + 4..off + 8].try_into().unwrap()) as usize;
            off += 32 + length * 4;
        }
        assert_eq!(
            found_win,
            Some(win_a.0),
            "XI2 release must reach the implicit-grab owner on the grab window"
        );
        assert!(state.pointer_grab.is_none(), "grab released after final release");
    }
```

- [ ] **Step 3: Run — expect BOTH to FAIL** (the first assertion in each: no implicit grab installs yet).

Run: `cargo test -p yserver-core implicit_grab_core_release_follows_press_recipient implicit_grab_xi2_release_follows_press_recipient`
Expected: 2 failures at the "must install the implicit grab" asserts. Commit the pinned bug:
```bash
git add crates/yserver-core/src/core_loop/pointer_fanout.rs
git commit -m "test(#94): pin missing implicit pointer grab (core + XI2) — failing"
```
(If the project convention objects to committing red tests, squash this with Task 3's commit instead — but run them red first.)

---

## Task 3: Capture + lifecycle (install / teardown)

**Files:** Modify `crates/yserver-core/src/core_loop/pointer_fanout.rs`; fix `process_request.rs:34255`.

- [ ] **Step 1: Capture struct + inner signature.** Above `pointer_event_fanout_to_state_inner`:

```rust
/// Delivery facts one fanout pass feeds the implicit-grab lifecycle —
/// Xorg `ActivateImplicitGrab`'s (client, pWin, deliveryMask, grabtype)
/// arguments (dix/events.c:2150), captured at the FIRST successful
/// natural press delivery (core before XI2, matching Xorg's
/// core-events-first order, :2415).
#[derive(Clone, Copy)]
struct DeliveredPress {
    owner: ClientId,
    /// Event window the press was delivered on (Xorg tempGrab->window).
    window: ResourceId,
    via_xi2: bool,
    /// Core deliveryMask (the recipient's window selection); 0 for XI2.
    core_mask: u32,
    /// Merged XI2 selection of ALL clients on `window`, snapshot at
    /// delivery (Xorg xi2mask_merge of the window's masks); 0 for core.
    xi2_mask: u32,
}

#[derive(Default)]
struct ImplicitGrabFanoutInfo {
    /// Event was withheld by the queue-while-frozen gate: nothing was
    /// delivered, and the `buttons_down` bookkeeping (which already ran)
    /// must not drive grab lifecycle. The lifecycle for this event runs
    /// when `xi1_compute_freezes` replays it.
    queued: bool,
    delivered_press: Option<DeliveredPress>,
}
```

Add `info: &mut ImplicitGrabFanoutInfo` as the last parameter of `pointer_event_fanout_to_state_inner` (the `#[allow(clippy::too_many_arguments)]` is already there).

- [ ] **Step 2: Set `queued` in the freeze-queue branch.** In the queue-while-frozen block (`:490-496`), before `return dropped;`:

```rust
        info.queued = true;
```

- [ ] **Step 3: Core capture.** In Step 4, immediately after the `core_targets` fanout (current `:992-1007`). The fanout call returns the DROPPED (dead/disconnecting) clients — a recipient that was not actually written must not become grab owner (Xorg installs only after a successful `TryClientEvents`, `events.c:2291-2308`; codex finding 5). Keep the `extras` binding visible:

```rust
        let extras = fanout_event_to_clients(state, &core_targets, |buf, seq, order| {
            /* ... existing encode_pointer_event call unchanged ... */
        });
        // Implicit-grab material: first successful natural press delivery,
        // core first (Xorg dix/events.c:2415 — "core events are delivered
        // first, an implicit grab may be activated on a core grab").
        // Window owner first = Xorg's DeliverToWindowOwner order; the
        // order among other same-window subscribers is "expressly
        // arbitrary" in Xorg too (events.c:2296). Dropped recipients
        // (write failed) are excluded.
        if event.kind == PointerEventKind::ButtonPress && info.delivered_press.is_none() {
            let owner = state
                .resources
                .window_owner(nested_id)
                .filter(|o| core_targets.contains(o) && !extras.contains(o))
                .or_else(|| core_targets.iter().find(|c| !extras.contains(c)).copied());
            if let Some(owner) = owner {
                let core_mask = state
                    .clients
                    .get(&owner.0)
                    .and_then(|c| c.event_masks.get(&nested_id).copied())
                    .unwrap_or(0);
                info.delivered_press = Some(DeliveredPress {
                    owner,
                    window: nested_id,
                    via_xi2: false,
                    core_mask,
                    xi2_mask: 0,
                });
            }
        }
        merge_dropped(&mut dropped, extras);
```

- [ ] **Step 4: XI2 capture.** In the per-recipient XI2 loop, AFTER the two-forms delivery for this `cid` (i.e. after the `for (deviceid, want) in forms { ... }` block, current `:1412-1496`), so a failed write disqualifies the recipient. Track drops locally inside the forms loop — where the existing code does `merge_dropped(&mut dropped, extras)` for each form write, also note whether `extras` named this `cid`:

```rust
        // (inside the forms loop, next to the existing merge_dropped)
        cid_dropped |= extras.contains(cid);
```

with `let mut cid_dropped = false;` declared before the forms loop, then after it:

```rust
        if event.kind == PointerEventKind::ButtonPress
            && !cid_dropped
            && info.delivered_press.is_none()
        {
            // Under a grab (xi2_grab_delivery) this records the grab owner,
            // and the lifecycle's no-grab gate then discards it — natural
            // delivery is the only path that can install. The xi2_mask
            // snapshot merges EVERY client's selection on the event window
            // (Xorg ActivateImplicitGrab xi2mask_merge of the window mask,
            // events.c:2183-2189).
            let merged: u32 = state
                .clients
                .values()
                .map(|c| {
                    [
                        XI2_SLAVE_POINTER_DEVICE_ID,
                        XI2_MASTER_POINTER_DEVICE_ID,
                        1,
                        0,
                    ]
                    .iter()
                    .filter_map(|d| c.xi2_masks.get(&(ev_win, *d)))
                    .fold(0u32, |m, v| m | v)
                })
                .fold(0u32, |m, v| m | v);
            info.delivered_press = Some(DeliveredPress {
                owner: *cid,
                window: ev_win,
                via_xi2: true,
                core_mask: 0,
                xi2_mask: merged,
            });
        }
```

- [ ] **Step 5: Lifecycle function** (module scope):

```rust
/// X11 implicit pointer grab lifecycle (Xorg ActivateImplicitGrab,
/// dix/events.c:2150-2193 + install site :2415-2421; release
/// Xi/exevents.c:1931-1958). Runs AFTER the fanout delivered the event:
/// the activating press is delivered pre-grab (delivery capture, not
/// rerouting) and the final release is delivered UNDER the grab before
/// deactivation — both matching Xorg's ordering.
fn implicit_pointer_grab_lifecycle(
    state: &mut ServerState,
    event: &HostPointerEvent,
    info: &ImplicitGrabFanoutInfo,
) {
    if info.queued {
        return;
    }
    match event.kind {
        PointerEventKind::ButtonPress => {
            // Xorg's whole gate is `if (deliveries) if (!grab ...)`: a
            // delivered press with no grab in effect. Deliberately NO
            // button-transition condition — Xorg has none, and the
            // XIReplayDevice replay (the #94 crux) re-enters with its
            // button bit already set from the original frozen delivery.
            let Some(press) = info.delivered_press else {
                return;
            };
            if state.pointer_grab.is_some()
                || state
                    .xi1_active_grabs
                    .contains_key(&crate::xinput::DEVICEID_SLAVE_POINTER)
            {
                return;
            }
            state.pointer_grab = Some((press.owner, press.window));
            state.pointer_grab_is_passive = false;
            state.active_pointer_grab = Some(crate::server::ActivePointerGrab {
                owner: press.owner,
                grab_window: press.window,
                // Xorg tempGrab->eventMask = deliveryMask; only the low
                // pointer bits participate in grab delivery gating.
                event_mask: (press.core_mask & 0xFFFF) as u16,
                cursor: ResourceId(0),
                time: event.time,
                // Core: OwnerGrabButton (1<<24) in the recipient's window
                // selection (dix/events.c:2174). XI2 masks have no
                // OwnerGrabButton (bit 24 is a raw-touch evtype) —
                // owner_events=false for the XI2 form.
                owner_events: !press.via_xi2 && press.core_mask & 0x0100_0000 != 0,
                via_xi2: press.via_xi2,
                implicit: true,
                xi2_mask: press.xi2_mask,
            });
            // Xorg ActivatePointerGrab updates grabTime on implicit
            // activation too (dix/events.c:1637): timestamp validation in
            // GrabPointer/UngrabPointer/AllowEvents must see this click.
            state.last_pointer_grab_time = event.time;
        }
        PointerEventKind::ButtonRelease => {
            // Xi/exevents.c:1931: deactivate when no buttons remain down,
            // after the release was delivered under the grab. Bare clear:
            // an implicit grab set no cursor override / confine / freeze /
            // crossing chain, so the explicit-grab teardown helpers
            // (NotifyUngrab chain, freeze-bridge release) must NOT run.
            if state.buttons_down == 0
                && state.active_pointer_grab.is_some_and(|g| g.implicit)
            {
                state.pointer_grab = None;
                state.active_pointer_grab = None;
            }
        }
        _ => {}
    }
}
```

- [ ] **Step 6: Wire the wrappers.** Both public entry points (`:45-62` and `:70-77`) become:

```rust
pub fn pointer_event_fanout_to_state(
    state: &mut ServerState,
    backend: &mut dyn crate::backend::Backend,
    xid_map: &HostXidMap,
    event: HostPointerEvent,
    handle_grabs: bool,
    is_replay: bool,
) -> Vec<ClientId> {
    let mut info = ImplicitGrabFanoutInfo::default();
    let dropped = pointer_event_fanout_to_state_inner(
        state, backend, xid_map, event, handle_grabs, is_replay, is_replay, &mut info,
    );
    implicit_pointer_grab_lifecycle(state, &event, &info);
    dropped
}
```

and in `replay_frozen_pointer_event_to_state` the same pattern around its `(state, backend, xid_map, event, false, false, true, &mut info)` call. (`event` is `Copy` — passing it by value to inner and by reference to the lifecycle is fine.)

- [ ] **Step 7: Run the Task 2 tests — expect PASS.**

Run: `cargo test -p yserver-core implicit_grab_core_release_follows_press_recipient implicit_grab_xi2_release_follows_press_recipient`
Expected: both PASS (Step-2 core redirect and the XI2 via_xi2 redirect already route under any active grab in the `handle_grabs=true` path).

- [ ] **Step 8: Full suite + triage.**

Run: `timeout 600 cargo test -p yserver-core`
Expected known failure: `xi_allow_events_replay_device_replays_frozen_button_press_to_target` at `process_request.rs:34255` — the replayed press now installs the implicit grab (the fix working). Update the assertions:

```rust
        assert_eq!(
            state.pointer_grab,
            Some((ClientId(TARGET_CLIENT_ID), ResourceId(TARGET_WIN))),
            "the replayed press must install the implicit grab on the natural \
             recipient (#94 — Xorg ActivateImplicitGrab on the replayed delivery)"
        );
        assert!(
            state
                .active_pointer_grab
                .is_some_and(|g| g.implicit && !g.via_xi2),
            "replayed core press installs a core-form implicit grab"
        );
        assert!(!state.pointer_grab_is_passive);
```

Triage every other failure against the "Known suite fallout" map above. Then:

- [ ] **Step 9: Gates + commit**

```bash
cargo +nightly fmt && cargo clippy --all-targets -- -D warnings && timeout 600 cargo test -p yserver-core
git add -A crates/yserver-core
git commit -m "feat(#94): implicit pointer grab — install on delivered press, auto-release on final ButtonRelease"
```

---

## Task 4: Redirects follow grab state (fast-click / queued-release path)

**Files:** Modify `crates/yserver-core/src/core_loop/pointer_fanout.rs:597, :1171`; new test.

- [ ] **Step 1: Drop `handle_grabs` from the two redirect gates.**

At `:597` (Step-2 core redirect):
```rust
    // Grab delivery is not stage-gated in Xorg: queued events drained by
    // PlayReleasedEvents deliver through DeliverGrabbedEvent while the
    // grab persists. `handle_grabs=false` re-entries (ReplayDevice/
    // ReplayPointer press replays, xi1_compute_freezes queue drains) must
    // still honor an active grab — the passive-grab MATCHING and freeze
    // QUEUE stay handle_grabs-gated (those are the re-entry hazards).
    // With no grab in effect, active_grab_target is None and this is the
    // exact pre-fix behavior for the replayed press.
    if let Some((grab_window, grab_client, gx, gy, owner_events, via_xi2, grab_event_mask)) =
        active_grab_target(state)
```

At `:1171` (XI2 redirect), remove the leading `handle_grabs &&` the same way (keep the crossing exclusion and everything else).

- [ ] **Step 2: Fix the XI1 `ReplayThisDevice` core-bridged TODO** (`process_request.rs:13805-13818` — codex finding 2). That branch deliberately leaves a core-bridged grab active before calling `replay_frozen_pointer_event_to_state` (`:13827`); with the redirect now honoring grab state, the replay would be redirected straight back to the grab it is supposed to bypass. Xorg's NOT_GRABBED deactivates the grab before replay regardless of grab kind (same rule `apply_allow_events` already implements for the core path at `:20021-20043`). Replace the `else` branch's warn-and-thaw with the same deactivation logic:

```rust
                        } else if state.pointer_grab_is_passive {
                            // Xorg NOT_GRABBED → DeactivateGrab before the
                            // replay, passive flavor (mirrors
                            // apply_allow_events, process_request.rs:20021).
                            let prev_grab_window = state.pointer_grab.map(|(_, w)| w);
                            state.pointer_grab = None;
                            state.pointer_grab_is_passive = false;
                            state.pointer_confine_to = ResourceId(0);
                            if let Some(prev) = prev_grab_window {
                                let to_win =
                                    crate::core_loop::key_fanout::deepest_window_at_pointer(state);
                                emit_core_pointer_grab_chain(state, prev, to_win, 2);
                            }
                            if let Some(f) = state.xi1_frozen.get_mut(&dev) {
                                f.state = Xi1SyncState::Thawed;
                            }
                        } else if state
                            .active_pointer_grab
                            .is_some_and(|g| g.owner == client_id)
                        {
                            // Explicit (or implicit) core grab: full
                            // deactivation, as apply_allow_events does at
                            // :20042.
                            deactivate_core_pointer_grab(state, backend, client_id);
                            if let Some(f) = state.xi1_frozen.get_mut(&dev) {
                                f.state = Xi1SyncState::Thawed;
                            }
                        } else {
                            // No core grab either — just thaw (previous
                            // behavior for the no-grab case).
                            if let Some(f) = state.xi1_frozen.get_mut(&dev) {
                                f.state = Xi1SyncState::Thawed;
                            }
                        }
```

(This is an XI1 `AllowDeviceEvents` path exercised mostly by XTS; the change also retires the `TODO` log-warn. Only the pointer device reaches this via `HostPointer` stored events, so `deactivate_core_pointer_grab` — pointer-specific — is the right helper; the `dev` here is the XI1 deviceid, keyboard events take the `HostKey`/`Xi1Routed` arms.)

- [ ] **Step 3: Fast-click test — queued release drains to the implicit-grab owner.** The Cinnamon shape: press frozen by the WM's sync grab, user releases *during* the freeze (queued), WM replays. Model the wire on `xi_allow_events_replay_device_replays_frozen_button_press_to_target` (process_request.rs:34141). Place in `process_request.rs` tests:

```rust
    /// #94 fast-click acceptance: press withheld by muffin's sync XI2
    /// passive grab; the RELEASE arrives during the freeze and is queued;
    /// XIAllowEvents(ReplayDevice) replays the press (installing the
    /// implicit grab on the natural recipient) and the queue drain must
    /// deliver the release UNDER that grab (Xorg PlayReleasedEvents →
    /// DeliverGrabbedEvent) — not re-hit-test it.
    #[test]
    fn xi_replay_device_then_queued_release_follows_implicit_grab() {
        use crate::{
            backend::Backend,
            host_x11::{HostPointerEvent, PointerEventKind},
            resources::ROOT_VISUAL,
        };
        const WM: u32 = 1;
        const APP: u32 = 2;
        const GRAB_WIN: u32 = 0x0010_0051;
        const APP_WIN: u32 = 0x0020_0052;
        const HOST_XID: u32 = 0xCAFE_0001;

        let mut state = ServerState::new();
        let _wm_peer = install_client(&mut state, WM);
        let mut app_peer = install_client(&mut state, APP);
        let mut backend = RecordingBackend::new();
        app_peer.set_nonblocking(true).expect("nonblocking");

        for (client, win) in [(WM, GRAB_WIN), (APP, APP_WIN)] {
            state.resources.create_window(
                ClientId(client),
                yserver_protocol::x11::CreateWindowRequest {
                    depth: 24,
                    window: ResourceId(win),
                    parent: ROOT_WINDOW,
                    x: 0,
                    y: 0,
                    width: 100,
                    height: 100,
                    border_width: 0,
                    class: 1,
                    visual: ROOT_VISUAL,
                    ..Default::default()
                },
            );
            let _ = state.resources.map_window(ResourceId(win));
        }
        state
            .clients
            .get_mut(&APP)
            .expect("app client")
            .event_masks
            .insert(ResourceId(APP_WIN), 0x0000_000c); // press|release
        Backend::register_top_level(&mut backend, None, ResourceId(APP_WIN), HOST_XID)
            .expect("register host xid");

        // Frozen sync passive grab held by the WM, activating press stored,
        // and the fast release already QUEUED (it arrived mid-freeze; the
        // queue gate decremented buttons_down and withheld it).
        state.pointer_grab = Some((ClientId(WM), ResourceId(GRAB_WIN)));
        state.pointer_grab_is_passive = true;
        let press = HostPointerEvent {
            kind: PointerEventKind::ButtonPress,
            host_xid: HOST_XID,
            detail: 1,
            time: 1000,
            root_x: 10,
            root_y: 10,
            event_x: 10,
            event_y: 10,
            state: 0,
            crossing_mode: 0,
            child: 0,
        };
        {
            let f = state
                .xi1_frozen
                .entry(crate::xinput::DEVICEID_SLAVE_POINTER)
                .or_default();
            f.state = crate::server::Xi1SyncState::FrozenWithEvent;
            f.stored = Some(crate::server::QueuedInputEvent::HostPointer(press));
        }
        let release = HostPointerEvent {
            kind: PointerEventKind::ButtonRelease,
            time: 1010,
            state: 0x100,
            ..press
        };
        state
            .sync_pending
            .push_back(crate::server::PendingSyncEvent {
                device: crate::xinput::DEVICEID_SLAVE_POINTER,
                event: crate::server::QueuedInputEvent::HostPointer(release),
            });

        // XIAllowEvents(ReplayDevice) from the WM.
        let mut body = Vec::with_capacity(8);
        body.extend_from_slice(&0u32.to_le_bytes()); // time
        body.extend_from_slice(&2u16.to_le_bytes()); // deviceid = master ptr
        body.push(2); // mode = ReplayDevice
        body.push(0); // pad
        let header = yserver_protocol::x11::RequestHeader {
            opcode: 131,
            data: 53,
            length_units: 3,
        };
        handle_xi2_request(
            &mut state,
            &mut backend,
            None,
            ClientId(WM),
            SequenceNumber(1),
            header,
            &body,
        )
        .expect("allow events");

        // The replayed press delivered to APP + the queued release drained
        // UNDER the freshly-installed implicit grab.
        let bytes = read_all_available(&mut app_peer);
        let (mut saw_press, mut saw_release) = (false, false);
        let mut off = 0usize;
        while off + 32 <= bytes.len() {
            match bytes[off] & 0x7F {
                4 => saw_press = true,
                5 => {
                    saw_release = true;
                    assert_eq!(
                        &bytes[off + 12..off + 16],
                        &APP_WIN.to_le_bytes(),
                        "queued release must deliver on the implicit grab window"
                    );
                }
                _ => {}
            }
            off += 32;
        }
        assert!(saw_press, "replayed press must reach the natural target");
        assert!(
            saw_release,
            "queued release must follow the implicit grab owner (fast click)"
        );
        assert!(
            state.pointer_grab.is_none() && state.active_pointer_grab.is_none(),
            "release completes the click — implicit grab torn down"
        );
    }
```

Note: `read_all_available` / `install_client` already exist in `process_request.rs` tests (the module `install_client` duplicates pointer_fanout's — see its comment). If `read_all_available` is missing there, copy pointer_fanout.rs:3045-3059 verbatim.

- [ ] **Step 4: Run new test + full suite; triage.**

Run: `cargo test -p yserver-core xi_replay_device_then_queued_release_follows_implicit_grab && timeout 600 cargo test -p yserver-core`
Expected: new test PASSES; if any existing replay/freeze test fails, check whether it relied on `handle_grabs=false` *bypassing an active grab* — if the grab should route it per Xorg (`PlayReleasedEvents` → `DeliverGrabbedEvent`), fix the test's expectation; report anything genuinely ambiguous instead of guessing.

- [ ] **Step 5: Gates + commit**

```bash
cargo +nightly fmt && cargo clippy --all-targets -- -D warnings && timeout 600 cargo test -p yserver-core
git add -A crates/yserver-core
git commit -m "fix(#94): active-grab redirect follows grab state on replay/queue-drain paths"
```

---

## Task 5: XI2 delivery under an implicit grab is mask-gated

**Files:** Modify the XI2 redirect in `crates/yserver-core/src/core_loop/pointer_fanout.rs` (`:1198-1205` region); new test.

- [ ] **Step 1: Gate the via_xi2 push on the grab's xi2_mask snapshot.** Replace the `if via_xi2 { ... }` body inside the `!owner_events || !target_qualifies_for_natural` branch:

```rust
            if via_xi2 {
                // Xorg grab delivery filters by GrabRec.xi2mask. For an
                // implicit grab that's the event window's merged XI2
                // selection snapshot at activation (ActivateImplicitGrab:
                // xi2mask_merge, events.c:2183-2189) — an implicit owner
                // that never selected XI_Motion must not start receiving
                // motion for the duration of every click. Explicit
                // XIGrabDevice grabs carry u32::MAX (wire mask not parsed
                // — pre-existing permissive delivery, unchanged).
                let grab_xi2_mask = state
                    .active_pointer_grab
                    .map_or(u32::MAX, |g| g.xi2_mask);
                if grab_xi2_mask & (1 << xi2_evtype) != 0 {
                    xi2_targets.push(grab_client);
                    nested_id = grab_window;
                    event_x = clamp_grab_coord(event.root_x, gx);
                    event_y = clamp_grab_coord(event.root_y, gy);
                }
            }
```

(`xi2_targets.clear()` and `xi2_grab_delivery = true` above it stay unconditional — the grab still *captures* unselected events, they just deliver to nobody, exactly like the core mask gate at `:684`. The `map_or(u32::MAX, ...)` covers passive grabs, which don't populate `active_pointer_grab` — their delivery already flows through the passive branch of `active_grab_target` and stays permissive here, exactly as today.)

- [ ] **Step 2: Test — motion not selected is captured, release still delivers.** In `pointer_fanout.rs` tests:

```rust
    /// Under a via_xi2 implicit grab, delivery is filtered by the owner's
    /// XI2 selection on the grab window (Xorg merges the window xi2mask
    /// into the implicit GrabRec): press+release-only selectors must not
    /// receive XI_Motion during the click, and the motion must not leak
    /// to the window under the cursor either (the grab captures it).
    #[test]
    fn implicit_grab_xi2_motion_filtered_by_owner_selection() {
        use yserver_protocol::x11::{CreateWindowRequest, ResourceId};
        const APP: u32 = 1;
        const OTHER: u32 = 2;
        let win_a = ResourceId(0x0010_0001);
        let win_b = ResourceId(0x0020_0001);
        const HOST_A: u32 = 0xCAFE_0001;
        const HOST_B: u32 = 0xCAFE_0002;

        let mut state = ServerState::new();
        let mut backend = RecordingBackend::default();
        let mut app_peer = install_client(&mut state, APP);
        let mut other_peer = install_client(&mut state, OTHER);

        for (client, win, x) in [(APP, win_a, 0i16), (OTHER, win_b, 500i16)] {
            state.resources.create_window(
                ClientId(client),
                CreateWindowRequest {
                    depth: 24,
                    window: win,
                    parent: ROOT_WINDOW,
                    x,
                    y: 0,
                    width: 100,
                    height: 100,
                    border_width: 0,
                    class: 1,
                    visual: crate::resources::ROOT_VISUAL,
                    ..Default::default()
                },
            );
            let _ = state.resources.map_window(win);
        }
        // APP: XI_ButtonPress(4)|XI_ButtonRelease(5) ONLY — no XI_Motion(6).
        state
            .clients
            .get_mut(&APP)
            .unwrap()
            .xi2_masks
            .insert((win_a, XI2_MASTER_POINTER_DEVICE_ID), (1 << 4) | (1 << 5));
        // OTHER selects XI_Motion on its own window — the grab must
        // capture the motion away from it anyway.
        state
            .clients
            .get_mut(&OTHER)
            .unwrap()
            .xi2_masks
            .insert((win_b, XI2_MASTER_POINTER_DEVICE_ID), 1 << 6);
        let mut xid_map = HostXidMap::new();
        xid_map.insert(HOST_A, win_a);
        xid_map.insert(HOST_B, win_b);

        let mut press = motion_event();
        press.kind = PointerEventKind::ButtonPress;
        press.host_xid = HOST_A;
        press.detail = 1;
        press.time = 1000;
        let _ =
            pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, press, true, false);
        assert!(
            state.active_pointer_grab.is_some_and(|g| g.implicit && g.via_xi2),
            "precondition: XI2 implicit grab installed"
        );
        let _ = read_all_available(&mut app_peer);
        let _ = read_all_available(&mut other_peer);

        // Motion over OTHER's window while the implicit grab is held.
        let mut motion = motion_event();
        motion.host_xid = HOST_B;
        motion.time = 1005;
        motion.root_x = 550;
        motion.root_y = 10;
        motion.event_x = 50;
        motion.event_y = 10;
        motion.state = 0x100;
        let _ =
            pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, motion, true, false);

        let xge_evtypes = |bytes: &[u8]| -> Vec<u16> {
            let mut found = Vec::new();
            let mut off = 0usize;
            while off + 32 <= bytes.len() {
                let advance = if bytes[off] == 35 {
                    found.push(u16::from_le_bytes([bytes[off + 8], bytes[off + 9]]));
                    32 + u32::from_le_bytes(bytes[off + 4..off + 8].try_into().unwrap())
                        as usize
                        * 4
                } else {
                    32
                };
                off += advance;
            }
            found
        };
        assert!(
            !xge_evtypes(&read_all_available(&mut app_peer)).contains(&6),
            "owner did not select XI_Motion on the grab window — the \
             implicit grab must not over-deliver it (Xorg xi2mask filter)"
        );
        assert!(
            xge_evtypes(&read_all_available(&mut other_peer)).is_empty(),
            "the grab captures motion — the window under the cursor gets nothing"
        );

        // The release (selected) still delivers to the owner on the grab window.
        let mut release = motion_event();
        release.kind = PointerEventKind::ButtonRelease;
        release.host_xid = HOST_B;
        release.detail = 1;
        release.time = 1010;
        release.root_x = 550;
        release.root_y = 10;
        release.event_x = 50;
        release.event_y = 10;
        release.state = 0x100;
        let _ =
            pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, release, true, false);
        assert!(
            xge_evtypes(&read_all_available(&mut app_peer)).contains(&5),
            "selected XI_ButtonRelease still delivers under the implicit grab"
        );
        assert!(state.pointer_grab.is_none(), "grab torn down after final release");
    }
```

- [ ] **Step 3: Run + gates + commit**

```bash
cargo test -p yserver-core implicit_grab_xi2 && timeout 600 cargo test -p yserver-core
cargo +nightly fmt && cargo clippy --all-targets -- -D warnings
git add -A crates/yserver-core
git commit -m "fix(#94): filter XI2 delivery under implicit grab by owner's selection"
```

---

## Task 6: Regression guards — request handlers + lifecycle edges

**Files:** Tests only (`pointer_fanout.rs` + `process_request.rs` test modules).

- [ ] **Step 1: Lifecycle edge tests** (pointer_fanout.rs tests). A shared scaffold keeps them short — add it once next to the tests:

```rust
    /// One mapped 100x100 window at (x,0) owned by `client`, host xid
    /// registered in `map`. Implicit-grab test scaffolding.
    fn implicit_test_window(
        state: &mut ServerState,
        map: &mut HostXidMap,
        client: u32,
        win: u32,
        host: u32,
        x: i16,
    ) -> yserver_protocol::x11::ResourceId {
        use yserver_protocol::x11::{CreateWindowRequest, ResourceId};
        let id = ResourceId(win);
        state.resources.create_window(
            ClientId(client),
            CreateWindowRequest {
                depth: 24,
                window: id,
                parent: ROOT_WINDOW,
                x,
                y: 0,
                width: 100,
                height: 100,
                border_width: 0,
                class: 1,
                visual: crate::resources::ROOT_VISUAL,
                ..Default::default()
            },
        );
        let _ = state.resources.map_window(id);
        map.insert(host, id);
        id
    }

    fn button_event(kind: PointerEventKind, host: u32, button: u8, time: u32) -> HostPointerEvent {
        let mut ev = motion_event();
        ev.kind = kind;
        ev.host_xid = host;
        ev.detail = button;
        ev.time = time;
        ev
    }
```

```rust
    /// Xorg gate is `if (deliveries)` (dix/events.c:2415): a press nobody
    /// selected installs nothing.
    #[test]
    fn implicit_grab_not_installed_when_press_undelivered() {
        let mut state = ServerState::new();
        let mut backend = RecordingBackend::default();
        let _peer = install_client(&mut state, 1);
        let mut xid_map = HostXidMap::new();
        // Window exists but no client selects button events anywhere.
        let _w = implicit_test_window(&mut state, &mut xid_map, 1, 0x0010_0001, 0xCAFE_0001, 0);
        let press = button_event(PointerEventKind::ButtonPress, 0xCAFE_0001, 1, 1000);
        let _ =
            pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, press, true, false);
        assert!(state.pointer_grab.is_none() && state.active_pointer_grab.is_none());
    }

    /// A press while an explicit grab is active never installs (Xorg
    /// `if (!grab ...)`) — and must not clobber the explicit record.
    #[test]
    fn implicit_grab_not_installed_under_explicit_grab() {
        use crate::server::ActivePointerGrab;
        let mut state = ServerState::new();
        let mut backend = RecordingBackend::default();
        let _peer = install_client(&mut state, 1);
        let mut xid_map = HostXidMap::new();
        let w = implicit_test_window(&mut state, &mut xid_map, 1, 0x0010_0001, 0xCAFE_0001, 0);
        state
            .clients
            .get_mut(&1)
            .unwrap()
            .event_masks
            .insert(w, 0x0000_000c);
        state.pointer_grab = Some((ClientId(1), w));
        state.pointer_grab_is_passive = false;
        let explicit = ActivePointerGrab {
            owner: ClientId(1),
            grab_window: w,
            event_mask: 0x000c,
            cursor: yserver_protocol::x11::ResourceId(0),
            time: 500,
            owner_events: false,
            via_xi2: false,
            implicit: false,
            xi2_mask: 0,
        };
        state.active_pointer_grab = Some(explicit);
        let press = button_event(PointerEventKind::ButtonPress, 0xCAFE_0001, 1, 1000);
        let _ =
            pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, press, true, false);
        assert!(
            state
                .active_pointer_grab
                .is_some_and(|g| !g.implicit && g.time == 500),
            "explicit grab record must be untouched by the press"
        );
        // And the explicit grab does NOT auto-release on the final release.
        let release = button_event(PointerEventKind::ButtonRelease, 0xCAFE_0001, 1, 1010);
        let _ =
            pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, release, true, false);
        assert!(
            state.pointer_grab.is_some(),
            "explicit grabs persist until UngrabPointer (only implicit auto-releases)"
        );
    }

    /// Multi-button click: the grab holds until ALL buttons release
    /// (Xi/exevents.c:1935 `!b->buttonsDown`), a second press neither
    /// reinstalls nor activates passive grabs (dix `if (!grab &&
    /// CheckDeviceGrabs...)`, yserver's active_grab_present gate), and
    /// pressing again after a partial release still doesn't reinstall
    /// (pins the no-transition-gate model).
    #[test]
    fn implicit_grab_multi_button_lifecycle() {
        let mut state = ServerState::new();
        let mut backend = RecordingBackend::default();
        let _peer = install_client(&mut state, 1);
        let _wm = install_client(&mut state, 2);
        let mut xid_map = HostXidMap::new();
        let w = implicit_test_window(&mut state, &mut xid_map, 1, 0x0010_0001, 0xCAFE_0001, 0);
        state
            .clients
            .get_mut(&1)
            .unwrap()
            .event_masks
            .insert(w, 0x0000_000c);
        // A passive grab that WOULD match button 3 — it must not activate
        // while the implicit grab holds the device.
        state.button_grabs.push(crate::server::PassiveButtonGrab {
            owner: ClientId(2),
            grab_window: w,
            button: 3,
            modifiers: 0x8000, // AnyModifier
            owner_events: false,
            event_mask: 0x0000_000c,
            pointer_mode: 1,
            keyboard_mode: 1,
            confine_to: yserver_protocol::x11::ResourceId(0),
            via_xi2: false,
        });

        let fan = |state: &mut ServerState, backend: &mut RecordingBackend, kind, button, time| {
            let ev = button_event(kind, 0xCAFE_0001, button, time);
            let _ = pointer_event_fanout_to_state(state, backend, &xid_map, ev, true, false);
        };
        fan(&mut state, &mut backend, PointerEventKind::ButtonPress, 1, 1000);
        assert!(state.active_pointer_grab.is_some_and(|g| g.implicit));
        fan(&mut state, &mut backend, PointerEventKind::ButtonPress, 3, 1005);
        assert!(
            state.active_pointer_grab.is_some_and(|g| g.implicit)
                && !state.pointer_grab_is_passive,
            "second press: no passive activation, implicit grab unchanged"
        );
        fan(&mut state, &mut backend, PointerEventKind::ButtonRelease, 1, 1010);
        assert!(
            state.pointer_grab.is_some(),
            "grab persists while button 3 is still down"
        );
        fan(&mut state, &mut backend, PointerEventKind::ButtonPress, 1, 1015);
        assert!(
            state.active_pointer_grab.is_some_and(|g| g.implicit && g.time == 1000),
            "re-press during the grab must not reinstall (time unchanged)"
        );
        fan(&mut state, &mut backend, PointerEventKind::ButtonRelease, 1, 1020);
        fan(&mut state, &mut backend, PointerEventKind::ButtonRelease, 3, 1025);
        assert!(
            state.pointer_grab.is_none() && state.active_pointer_grab.is_none(),
            "final release tears down"
        );
    }
```

- [ ] **Step 2: Request-handler tests** (process_request.rs tests; wire idiom from `grab_pointer_skips_synthesised_crossing_when_mask_unselected`, process_request.rs:44622 — GrabPointer body = window(4) event-mask(2) pointer-mode(1) keyboard-mode(1) confine-to(4) cursor(4) time(4), owner_events in `header.data`; reply byte 0 = 1, status = byte 1):

```rust
    /// An implicit grab is a REAL grab for request purposes (Xorg shares
    /// deviceGrab.grab): another client's GrabPointer during it fails
    /// AlreadyGrabbed (events.c:5240-5243); the owner's GrabPointer
    /// replaces it; the owner's UngrabPointer releases it (events.c:5155).
    #[test]
    fn implicit_grab_interacts_with_grab_requests_like_a_real_grab() {
        use crate::server::ActivePointerGrab;
        const OWNER: u32 = 1;
        const INTRUDER: u32 = 2;
        const WIN: u32 = 0x0010_0001;

        let mut state = ServerState::new();
        let mut backend = RecordingBackend::new();
        let mut owner_peer = install_client(&mut state, OWNER);
        let mut intruder_peer = install_client(&mut state, INTRUDER);
        owner_peer.set_nonblocking(true).unwrap();
        intruder_peer.set_nonblocking(true).unwrap();
        state.resources.create_window(
            ClientId(OWNER),
            yserver_protocol::x11::CreateWindowRequest {
                depth: 24,
                window: ResourceId(WIN),
                parent: ROOT_WINDOW,
                x: 0,
                y: 0,
                width: 100,
                height: 100,
                border_width: 0,
                class: 1,
                visual: crate::resources::ROOT_VISUAL,
                ..Default::default()
            },
        );
        let _ = state.resources.map_window(ResourceId(WIN));

        let install_implicit = |state: &mut ServerState| {
            state.pointer_grab = Some((ClientId(OWNER), ResourceId(WIN)));
            state.pointer_grab_is_passive = false;
            state.active_pointer_grab = Some(ActivePointerGrab {
                owner: ClientId(OWNER),
                grab_window: ResourceId(WIN),
                event_mask: 0x000c,
                cursor: ResourceId(0),
                time: 1000,
                owner_events: false,
                via_xi2: false,
                implicit: true,
                xi2_mask: 0,
            });
            state.last_pointer_grab_time = 1000;
        };
        let grab_body = || {
            let mut body = Vec::with_capacity(20);
            body.extend_from_slice(&WIN.to_le_bytes());
            body.extend_from_slice(&0u16.to_le_bytes()); // event-mask
            body.push(1); // pointer-mode async
            body.push(1); // keyboard-mode async
            body.extend_from_slice(&0u32.to_le_bytes()); // confine-to
            body.extend_from_slice(&0u32.to_le_bytes()); // cursor
            body.extend_from_slice(&0u32.to_le_bytes()); // time CurrentTime
            body
        };
        let header = RequestHeader {
            opcode: 26,
            data: 0,
            length_units: 6,
        };
        let grab_status = |peer: &mut std::os::unix::net::UnixStream| -> u8 {
            let wire = read_all_available(peer);
            assert!(wire.len() >= 32, "expected a GrabPointer reply");
            assert_eq!(wire[0], 1, "reply, not an event/error");
            wire[1]
        };

        // (a) Another client's GrabPointer → AlreadyGrabbed, state untouched.
        install_implicit(&mut state);
        handle_grab_pointer(
            &mut state,
            &mut backend,
            ClientId(INTRUDER),
            SequenceNumber(1),
            header,
            &grab_body(),
        )
        .expect("grab pointer");
        assert_eq!(grab_status(&mut intruder_peer), 1, "AlreadyGrabbed");
        assert!(
            state
                .active_pointer_grab
                .is_some_and(|g| g.implicit && g.owner == ClientId(OWNER)),
            "implicit grab untouched by the failed request"
        );

        // (b) The owner's GrabPointer replaces its implicit grab.
        handle_grab_pointer(
            &mut state,
            &mut backend,
            ClientId(OWNER),
            SequenceNumber(2),
            header,
            &grab_body(),
        )
        .expect("grab pointer");
        assert_eq!(grab_status(&mut owner_peer), 0, "GrabSuccess");
        assert!(
            state.active_pointer_grab.is_some_and(|g| !g.implicit),
            "explicit grab replaced the implicit one"
        );

        // (c) Owner's UngrabPointer releases its own implicit grab; a
        // non-owner's UngrabPointer is a no-op.
        install_implicit(&mut state);
        let ungrab_body = 0u32.to_le_bytes(); // time CurrentTime
        handle_ungrab_pointer(
            &mut state,
            &mut backend,
            ClientId(INTRUDER),
            SequenceNumber(3),
            &ungrab_body,
        )
        .expect("ungrab pointer");
        assert!(
            state.pointer_grab.is_some(),
            "non-owner UngrabPointer must not touch the implicit grab"
        );
        handle_ungrab_pointer(
            &mut state,
            &mut backend,
            ClientId(OWNER),
            SequenceNumber(4),
            &ungrab_body,
        )
        .expect("ungrab pointer");
        assert!(
            state.pointer_grab.is_none() && state.active_pointer_grab.is_none(),
            "owner UngrabPointer releases the implicit grab (Xorg SameClient)"
        );
    }
```

- [ ] **Step 3: Full suite + gates + commit**

```bash
timeout 600 cargo test -p yserver-core && cargo +nightly fmt && cargo clippy --all-targets -- -D warnings
git add -A crates/yserver-core
git commit -m "test(#94): implicit grab regression guards (handlers + lifecycle edges)"
```

---

## Task 7: Cinnamon-shaped acceptance — slow click with tree mutation

**Files:** Test in `process_request.rs` tests (fast click already covered by Task 4's test).

- [ ] **Step 1: Slow-click acceptance test.** Same wire idioms as Task 4's test; APP selects XI2 only (the true Steam shape). Uses `state.resources.reparent_window(ReparentWindowRequest { window, parent, x, y })` for the muffin-style mutation.

```rust
    /// #94 slow-click acceptance (the traced Cinnamon shape): muffin's
    /// sync XI2 passive grab withholds the press; XIAllowEvents(
    /// XIReplayDevice) re-delivers it to Steam (XI2-only selector),
    /// installing the implicit grab; muffin then MUTATES THE TREE
    /// (click-to-focus reparents/restacks) before the user releases.
    /// The release must follow the implicit grab to Steam — pre-fix it
    /// was re-hit-tested against the mutated tree and lost (steam.xtrace:
    /// 164 XI2 presses vs 28 releases → stuck button).
    #[test]
    fn xi_replay_device_press_then_tree_mutation_release_follows_grab() {
        use crate::{
            backend::Backend,
            core_loop::pointer_fanout::pointer_event_fanout_to_state,
            host_x11::{HostPointerEvent, PointerEventKind},
            resources::ROOT_VISUAL,
        };
        const WM: u32 = 1;
        const APP: u32 = 2;
        const GRAB_WIN: u32 = 0x0010_0051; // WM's passive-grab window
        const APP_WIN: u32 = 0x0020_0052; // Steam-like, XI2-only selector
        const FRAME_WIN: u32 = 0x0010_0060; // WM frame created mid-click
        const COVER_WIN: u32 = 0x0010_0061; // WM window the release re-hits
        const HOST_APP: u32 = 0xCAFE_0001;
        const HOST_COVER: u32 = 0xCAFE_0002;
        const XI2_MASTER: u16 = 2;

        let mut state = ServerState::new();
        let mut wm_peer = install_client(&mut state, WM);
        let mut app_peer = install_client(&mut state, APP);
        let mut backend = RecordingBackend::new();
        wm_peer.set_nonblocking(true).unwrap();
        app_peer.set_nonblocking(true).unwrap();

        for (client, win) in [(WM, GRAB_WIN), (APP, APP_WIN), (WM, FRAME_WIN), (WM, COVER_WIN)] {
            state.resources.create_window(
                ClientId(client),
                yserver_protocol::x11::CreateWindowRequest {
                    depth: 24,
                    window: ResourceId(win),
                    parent: ROOT_WINDOW,
                    x: 0,
                    y: 0,
                    width: 100,
                    height: 100,
                    border_width: 0,
                    class: 1,
                    visual: ROOT_VISUAL,
                    ..Default::default()
                },
            );
            let _ = state.resources.map_window(ResourceId(win));
        }
        // Steam shape: cooked XI2 buttons, NO core mask.
        state
            .clients
            .get_mut(&APP)
            .expect("app")
            .xi2_masks
            .insert((ResourceId(APP_WIN), XI2_MASTER), (1 << 4) | (1 << 5));
        // The WM selects XI2 buttons on the covering window — a re-hit-test
        // would deliver the release THERE.
        state
            .clients
            .get_mut(&WM)
            .expect("wm")
            .xi2_masks
            .insert((ResourceId(COVER_WIN), XI2_MASTER), (1 << 4) | (1 << 5));
        Backend::register_top_level(&mut backend, None, ResourceId(APP_WIN), HOST_APP)
            .expect("register app");
        Backend::register_top_level(&mut backend, None, ResourceId(COVER_WIN), HOST_COVER)
            .expect("register cover");

        // Frozen sync passive grab held by the WM, press stored.
        state.pointer_grab = Some((ClientId(WM), ResourceId(GRAB_WIN)));
        state.pointer_grab_is_passive = true;
        let press = HostPointerEvent {
            kind: PointerEventKind::ButtonPress,
            host_xid: HOST_APP,
            detail: 1,
            time: 1000,
            root_x: 10,
            root_y: 10,
            event_x: 10,
            event_y: 10,
            state: 0,
            crossing_mode: 0,
            child: 0,
        };
        {
            let f = state
                .xi1_frozen
                .entry(crate::xinput::DEVICEID_SLAVE_POINTER)
                .or_default();
            f.state = crate::server::Xi1SyncState::FrozenWithEvent;
            f.stored = Some(crate::server::QueuedInputEvent::HostPointer(press));
        }

        // XIAllowEvents(ReplayDevice) from the WM.
        let mut body = Vec::with_capacity(8);
        body.extend_from_slice(&0u32.to_le_bytes());
        body.extend_from_slice(&2u16.to_le_bytes());
        body.push(2); // ReplayDevice
        body.push(0);
        handle_xi2_request(
            &mut state,
            &mut backend,
            None,
            ClientId(WM),
            SequenceNumber(1),
            yserver_protocol::x11::RequestHeader {
                opcode: 131,
                data: 53,
                length_units: 3,
            },
            &body,
        )
        .expect("allow events");

        let xge_events = |bytes: &[u8]| -> Vec<(u16, u32)> {
            let mut found = Vec::new();
            let mut off = 0usize;
            while off + 32 <= bytes.len() {
                let advance = if bytes[off] == 35 {
                    found.push((
                        u16::from_le_bytes([bytes[off + 8], bytes[off + 9]]),
                        u32::from_le_bytes(bytes[off + 24..off + 28].try_into().unwrap()),
                    ));
                    32 + u32::from_le_bytes(bytes[off + 4..off + 8].try_into().unwrap())
                        as usize
                        * 4
                } else {
                    32
                };
                off += advance;
            }
            found
        };
        assert!(
            xge_events(&read_all_available(&mut app_peer))
                .contains(&(4, APP_WIN)),
            "replayed press must reach APP as XI2 on its window"
        );
        assert!(
            state
                .active_pointer_grab
                .is_some_and(|g| g.implicit
                    && g.via_xi2
                    && g.owner == ClientId(APP)
                    && g.grab_window == ResourceId(APP_WIN)),
            "replayed press installs the via_xi2 implicit grab (#94 crux)"
        );

        // muffin-style mutation between press and release: reparent the
        // app window into a WM frame and let a WM window cover the spot.
        let _ = state
            .resources
            .reparent_window(yserver_protocol::x11::ReparentWindowRequest {
                window: ResourceId(APP_WIN),
                parent: ResourceId(FRAME_WIN),
                x: 0,
                y: 0,
            });
        let _ = state.resources.map_window(ResourceId(APP_WIN));

        // Natural release now resolves over the WM's covering window.
        let xid_map = backend.xid_map().clone();
        let release = HostPointerEvent {
            kind: PointerEventKind::ButtonRelease,
            host_xid: HOST_COVER,
            time: 1200,
            state: 0x100,
            ..press
        };
        let _ =
            pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, release, true, false);

        assert!(
            xge_events(&read_all_available(&mut app_peer))
                .contains(&(5, APP_WIN)),
            "release must follow the implicit grab to APP on the grab window"
        );
        assert!(
            !xge_events(&read_all_available(&mut wm_peer))
                .iter()
                .any(|(evtype, _)| *evtype == 5),
            "the grab captures the release away from the re-hit-tested window"
        );
        assert!(
            state.pointer_grab.is_none() && state.active_pointer_grab.is_none(),
            "click complete — implicit grab torn down"
        );
    }
```

- [ ] **Step 2: Run + full suite + gates + commit**

```bash
timeout 600 cargo test -p yserver-core && cargo +nightly fmt && cargo clippy --all-targets -- -D warnings
git add -A crates/yserver-core
git commit -m "test(#94): Cinnamon-shaped replay + tree-mutation acceptance"
```

---

## Task 8: Convergence review, HW smoke, secondary-bug decision, merge gate

- [ ] **Step 1: Toolchain gates once more** — `cargo +nightly fmt`; `cargo clippy --all-targets -- -D warnings`; `timeout 600 cargo test -p yserver-core`; also `cargo test --workspace` if the branch's earlier commits ran it.

- [ ] **Step 2: codex review of the diff** (Skill `codex`, per project CLAUDE.md): ask it to verify the implementation against `../xserver` anchors above — especially the install gate (no transition), teardown ordering (deliver-then-clear), the redirect-gate change's blast radius on `xi1_compute_freezes` drains, and the `:34255` assertion flip. Address findings; re-review until convergence.

- [ ] **Step 3: HW smoke (the REAL gate — `feedback_no_commit_before_smoke` applies to the MERGE, branch commits are fine).** User-driven on HW:
  - **Cinnamon (muffin) + Steam**: clicks on unfocused Steam windows register reliably (was ~1/20); menus/nav/CSD buttons work; no stuck buttons.
  - **Regressions**: MATE (marco — its CORE sync grab path must still work), XFCE, i3: clicks, drags (window move/resize — these NOW run under implicit grabs when the WM doesn't grab), GTK menus, scroll wheel (buttons 4-7 now briefly install implicit grabs — verify scrolling in GTK/Qt/SDL apps), double-click, rubber-band select on desktop.
  - If clicks still miss: `RUST_LOG=yserver_core::core_loop::pointer_fanout=trace` and check the release recipient vs press recipient; `yserver::input::clickhit=trace` shows the redirect decision.

- [ ] **Step 4: Secondary XI2 leaf/ancestor double-delivery decision.** If HW is green with it present, record in `docs/known-issues.md` (mechanics only — user writes prose): one XI2 dispatch delivers to leaf AND ancestor; Xorg stops at the first successfully-delivering window (`events.c:2885-2930`); fix locus `compute_xi2_targets` (`pointer_fanout.rs:2623`) / per-window emit. If it still breaks Steam, open a follow-up task — NOT part of this branch.

- [ ] **Step 4b: KNOWN FOLLOW-UP — XI1 GrabDeviceButton vs implicit grab (codex final review, 2026-07-15).** Because the implicit grab installs in the fanout wrapper AFTER the whole fanout (incl. the XI1 device-event fanout at `pointer_fanout.rs:~1094`), a legacy XI1 `GrabDeviceButton` passive grab from another client can activate on the SAME press (`xi1_route_device_event` passive match at `~1916`, which does not consult `pointer_grab`), populate `xi1_active_grabs[SLAVE_POINTER]`, and the lifecycle install gate (`xi1_active_grabs.contains_key(SLAVE_POINTER)`) then declines the implicit grab — so the release follows the XI1 grab, diverging from Xorg's core-first "core implicit grab stops subsequent XI delivery" (`dix/events.c:2415-2422`). **NOT a regression** (pre-branch there was no implicit grab at all, so the XI1-grab client is unaffected; this is only a gap in the NEW feature). **Niche** (XI1 `GrabDeviceButton` is legacy; not the #94 XI2/core scenario; not reachable by the Cinnamon HW smoke). **Deferred** — the faithful fix teaches the XI1 passive matcher to respect an active core/implicit pointer grab (Xorg `CheckDeviceGrabs` runs only `if (!grab)`), which needs its own tests + review. Track as a separate conformance task; does NOT block this branch.

- [ ] **Step 5: Finish the branch** — only after HW confirms Cinnamon: squash-merge `codex/freeze-state-unification` with user confirmation (AGENTS.md); user updates `docs/known-issues.md` prose for the fixed items. Update memory (`project_issue94_steam_xi_residuals`, plan memory) to the merged state.

---

## Self-Review Notes (for the executor)

- **The install gate is `delivered && no grab` — nothing else.** If you find yourself adding a `prior_buttons_down` condition, re-read `dix/events.c:2415-2421`; the transition gate breaks the ReplayDevice crux (bit already set on replay).
- **"Delivered" means WRITTEN** — a recipient the fanout dropped (dead socket) must not become grab owner (codex finding 5): the core capture filters by the fanout's dropped list, the XI2 capture sits after the forms loop behind `!cid_dropped`.
- **Lifecycle runs in the WRAPPERS, after inner returns** — never inside inner (three early returns: queue `:496`, is_replay `:1017`, no-top-level `:1080`; the wrapper covers all of them; `queued` skips the withheld case).
- **Teardown is a bare clear** — `deactivate_core_pointer_grab` / `release_passive_grab_on_button_release` emit crossings and release freeze bridges that don't apply; reusing them double-delivers crossings.
- **`UngrabPointer`/`GrabPointer`/`XIUngrabDevice`/`AllowEvents`/disconnect need NO changes** — audited; owner-keyed checks give Xorg semantics with the implicit grab in the shared slot. Task 6 pins them. The ONE handler change is the XI1 `ReplayThisDevice` core-bridged branch (Task 4 Step 2, codex finding 2). `XIGrabDevice`'s always-succeed clobbering is a pre-existing divergence left out of scope (design decision 1).
- **Redirect gates lose `handle_grabs`, passive matching and freeze-queueing keep it** (Task 4) — mixing these up either re-wedges the freeze path or re-breaks the fast click.
- **The XI2 grab mask is a SNAPSHOT at activation** (`xi2_mask` field, merged over all clients on the window — codex finding 4 / `events.c:2183-2189`), not a live `xi2_masks` lookup at delivery.
- **`last_pointer_grab_time` IS updated** on implicit activation (Xorg grabTime — v3 note 4 of the OLD plan said don't touch it; that was wrong, see `events.c:1637`).
- **Expect and triage suite fallout** — the `:34255` flip is the fix working (codex round-1 found no other confirmed breakage, but re-verify by running); judge every failure against "what would Xorg do with the grab active".
- **Nothing merges before Cinnamon HW smoke.**
