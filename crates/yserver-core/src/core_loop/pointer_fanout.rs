//! State-borrowing replacement for `server::pointer_event_fanout`.
//!
//! Mirrors the pre-lift logic from `server.rs`:
//!   * translate root_x/root_y from host-screen coords to ynest-root,
//!   * honour any active or passive pointer grab,
//!   * walk the propagation chain for core device events,
//!   * fan out parallel XI2 device + raw events to clients selecting
//!     XI2 masks on the target / top-level / root.
//!
//! All work happens inside a single `&mut ServerState` borrow scope —
//! per-target writers go through `client_io::write_or_buffer` so the
//! D3 lift can wire the disconnect list back into the core loop.
//!
//! The xid_map is still passed as `Arc<Mutex<HostXidMap>>`. Phase F1
//! demotes it to a plain field on `HostX11Backend` and at that point
//! the helper takes `&HostXidMap`.

use yserver_protocol::x11::{self, ClientId, ResourceId, SequenceNumber};

use crate::{
    core_loop::fanout::{
        client_target_id, fanout_event_to_clients, pointer_propagation_target_by_id,
    },
    host_x11::{HostPointerEvent, HostXidMap, PointerEventKind},
    resources::ROOT_WINDOW,
    server::{ServerState, xi2_mask_for_client},
};

const XI2_MAJOR_OPCODE: u8 = 137;
const XI2_MASTER_POINTER_DEVICE_ID: u16 = 2;
const XI2_SLAVE_POINTER_DEVICE_ID: u16 = 4;

/// Fan a host pointer event out to nested clients.
///
/// `handle_grabs` toggles passive-grab matching and active-grab
/// redirection. Pass `false` from `AllowEvents ReplayPointer` to avoid
/// re-checking the same passive grab that was just released.
///
/// `is_replay` is set when the call comes from the core
/// `AllowEvents(ReplayPointer)` re-delivery path. That path keeps XI2
/// fanout suppressed because the original physical event has already
/// been delivered to XI2 listeners. XI2 replay after `XIAllowEvents`
/// uses `is_replay=false` because synchronous XI2 passive grabs must
/// re-deliver the device event to the natural target.
pub fn pointer_event_fanout_to_state(
    state: &mut ServerState,
    backend: &mut dyn crate::backend::Backend,
    xid_map: &HostXidMap,
    event: HostPointerEvent,
    handle_grabs: bool,
    is_replay: bool,
) -> Vec<ClientId> {
    let now = std::time::Instant::now();
    // Capture priors BEFORE mutating; needed by the IDLETIME wake handler.
    #[allow(clippy::cast_possible_truncation)]
    let prior_global = now
        .duration_since(state.dpms.last_activity)
        .as_millis()
        .min(u128::from(u32::MAX)) as i64;
    // XI2 master device IDs are always small (2 here); cast u16 → u8 is safe.
    // Per-device prior: fall back to global if no per-device entry yet.
    // Matches `idletime_baseline`'s fallback (server.rs Task 1) — without
    // this, the very first input event for a device whose baseline isn't
    // recorded would compute prior_device=0 and a per-device Negative
    // alarm (whose wait_value > 0) would not see the `old > wait` half of
    // its trigger.
    let prior_device = state
        .per_device_last_activity
        .get(&(XI2_MASTER_POINTER_DEVICE_ID as u8))
        .copied()
        .map(|t| {
            #[allow(clippy::cast_possible_truncation)]
            let v = now.duration_since(t).as_millis().min(u128::from(u32::MAX)) as i64;
            v
        })
        .unwrap_or(prior_global);

    state.dpms.last_activity = now;
    state
        .per_device_last_activity
        .insert(XI2_MASTER_POINTER_DEVICE_ID as u8, now);

    // IDLETIME wake: fires Negative-* alarms before the input event itself
    // reaches clients (predictable ordering).
    crate::core_loop::process_request::evaluate_idletime_negative_alarms_on_input_wake(
        state,
        XI2_MASTER_POINTER_DEVICE_ID as u8,
        prior_global,
        prior_device,
    );

    if state.dpms.enabled && state.dpms.power_level != 0 {
        crate::core_loop::process_request::apply_dpms_transition(state, backend, 0);
        // DPMS coupling tail already flipped SS Off if it was On.
    }
    if matches!(
        state.screensaver.active,
        crate::server::ScreenSaverActive::On
    ) {
        // Standalone SS activation (DPMS was On already; SS came up
        // via idle timer or ForceScreenSaver) — input wakes it.
        crate::core_loop::process_request::apply_screen_saver_transition(
            state,
            backend,
            crate::server::ScreenSaverActive::Off,
            /*forced=*/ false,
        );
    }

    let mut dropped = Vec::new();

    // Step 1 — translate host-screen coords to ynest-root coords.
    let event = translate_host_event(state, xid_map, event);

    // Cache the pointer position so server-generated events that must
    // carry it (XI2 focus events) don't ship (0,0). Mirrors Xorg keeping
    // the sprite position in device state.
    state.pointer_root = (event.root_x, event.root_y);

    // Sync-passive-grab freeze queue (Xorg `dix/events.c:1320`
    // ComputeFreezes + PlayReleasedEvents). While a sync passive
    // grab is frozen — between the activating press and
    // AllowEvents — subsequent pointer events MUST NOT leak to
    // the natural target. Marco does ~10 round-trips of focus and
    // property work between the press and AllowEvents(ReplayPointer);
    // a fast user release in that window would otherwise reach the
    // app before the replayed press, malforming the gesture
    // (menus + titlebar drags break). Queue them here for replay.
    //
    // Crossings (Enter/Leave) and the replay path itself bypass —
    // crossings are pointer-tracking notifications Xorg doesn't
    // queue, and the replay re-entry mustn't recursively re-queue.
    if !is_replay
        && handle_grabs
        && state.pointer_grab_is_passive
        && state.frozen_pointer_event.is_some()
        && !matches!(
            event.kind,
            PointerEventKind::EnterNotify | PointerEventKind::LeaveNotify,
        )
    {
        log::trace!(
            "pointer_fanout: QUEUE-WHILE-FROZEN kind={:?} button={} root=({},{}) queue_len={}",
            event.kind,
            event.detail,
            event.root_x,
            event.root_y,
            state.frozen_pointer_queue.len() + 1,
        );
        state.frozen_pointer_queue.push_back(event);
        return dropped;
    }

    if matches!(
        event.kind,
        PointerEventKind::ButtonPress | PointerEventKind::ButtonRelease
    ) {
        log::trace!(
            "pointer_fanout entry: kind={:?} button={} host_xid=0x{:x} root=({},{}) event_xy=({},{})",
            event.kind,
            event.detail,
            event.host_xid,
            event.root_x,
            event.root_y,
            event.event_x,
            event.event_y,
        );
    }

    // Resolve the actual hit window (deepest mapped child under cursor)
    // up front. We need it for both the core-event paths below (passive
    // grab matching, normal propagation) and for the XI2 fanout.
    let root_hit = state.root_pointer_target_at(event.root_x, event.root_y);
    let top_level_id_opt = root_hit
        .map(|(target, _, _)| state.top_level_for_target(target))
        .or_else(|| xid_map.get(&event.host_xid).copied());
    let top_level_id = top_level_id_opt.unwrap_or(ROOT_WINDOW);
    let (target, target_x, target_y) = root_hit.unwrap_or_else(|| {
        xid_map
            .get(&event.host_xid)
            .copied()
            .and_then(|tl| {
                state
                    .pointer_target_at(tl, event.event_x, event.event_y)
                    .or(Some((tl, event.event_x, event.event_y)))
            })
            .unwrap_or((ROOT_WINDOW, event.event_x, event.event_y))
    });

    if matches!(
        event.kind,
        PointerEventKind::ButtonPress | PointerEventKind::ButtonRelease
    ) {
        crate::core_loop::grab_debug::log(
            state,
            &format!(
                "BTN {:?} btn={} root=({},{}) hit_target=0x{:x} top_level=0x{:x} handle_grabs={} is_replay={}",
                event.kind,
                event.detail,
                event.root_x,
                event.root_y,
                target.0,
                top_level_id.0,
                handle_grabs,
                is_replay,
            ),
        );
    }

    // ── Core fanout ─────────────────────────────────────────────────
    let mut handled_core_via_grab = false;

    // Step 2 — active-grab redirection (core events only).
    if handle_grabs
        && let Some((grab_window, grab_client, gx, gy, owner_events)) = active_grab_target(state)
    {
        // With `owner_events=true`, pointer events on windows owned
        // by the grab client are reported normally (to the deepest
        // natural window) rather than redirected to `grab_window`.
        // The grab itself is just an exclusivity mechanism — other
        // clients can't see the events. GTK3 menus rely on this:
        // motion fires on the panel button until the user actually
        // crosses into the popup, at which point natural
        // Enter/Leave fire and GTK3 transitions menu state. With
        // `owner_events=false`, all events report against grab_window.
        // Step-2 active-grab redirect — `owner_events=true` semantics
        // per X11 spec: deliver normally if the natural target's
        // window is OWNED BY the grab client, in addition to the
        // topological "within grab subtree" case. The earlier check
        // used pure topology, which worked for Cinnamon's muffin
        // (grab window = the same window the menu sub-widgets are
        // descendants of) but failed on MATE's mate-panel pattern
        // where menu items are SEPARATE TOP-LEVEL override-redirect
        // windows OWNED BY mate-panel — siblings of the panel main
        // window, not descendants. Pre-fix this redirected hover
        // motion to the panel main window with grab-relative coords,
        // GTK couldn't localise the hover, submenus stopped opening.
        // No-op when owner_events=false (Cinnamon's pattern).
        let target_qualifies_for_natural = target == grab_window
            || state.resources.is_descendant_of(target, grab_window)
            || state.resources.window_owner(target) == Some(grab_client);
        let redirect_to_grab = !owner_events || !target_qualifies_for_natural;
        if !matches!(
            event.kind,
            PointerEventKind::EnterNotify | PointerEventKind::LeaveNotify
        ) && redirect_to_grab
        {
            let event_x = clamp_grab_coord(event.root_x, gx);
            let event_y = clamp_grab_coord(event.root_y, gy);
            log::trace!(
                "pointer_fanout: ACTIVE-GRAB redirect kind={:?} button={} grab_window=0x{:x} grab_client={:?} owner_events={}",
                event.kind,
                event.detail,
                grab_window.0,
                grab_client,
                owner_events,
            );
            if matches!(
                event.kind,
                PointerEventKind::ButtonPress | PointerEventKind::ButtonRelease
            ) {
                crate::core_loop::grab_debug::log(
                    state,
                    &format!(
                        "  -> route=ACTIVE-GRAB redirect to grab_window=0x{:x} client={:?}",
                        grab_window.0, grab_client
                    ),
                );
            }
            let extras = fanout_event_to_clients(state, &[grab_client], |buf, seq, order| {
                encode_pointer_event(
                    buf,
                    order,
                    event.kind,
                    seq,
                    event.detail,
                    event.time,
                    grab_window,
                    ResourceId(0), // active-grab redirect: no propagation child
                    event,
                    event_x,
                    event_y,
                );
            });
            merge_dropped(&mut dropped, extras);
            handled_core_via_grab = true;
            // Else (owner_events=true and target owned by grab client):
            // fall through to normal propagation so the event fires on
            // the natural window via the usual subscriber-walk path.
        }
        // For Enter/Leave (natural pointer crossings between windows),
        // never mark handled_core_via_grab — let them fall through to
        // the normal core propagation in step 4. Pre-fix the existing
        // code set the flag unconditionally and dropped natural
        // crossings entirely while a grab was active, so GTK3 menus
        // (xfce4-panel's main menu, marco's title-bar popup) never
        // received the "pointer entered me" notification needed to
        // engage hover/click tracking. Matches Xorg
        // `dix/events.c::DeliverGrabbedEvent` which only re-routes
        // pointer events explicitly listed in the grab's event mask.
    }

    // Passive button grabs must end on the matching release even when
    // `owner_events=true` keeps the press/release on the owned window.
    // Releasing here keeps the grab lifecycle aligned with Xorg and
    // avoids pinning the dialog in a grabbed state until its own
    // timeout path gives up.
    release_passive_grab_on_button_release(state, event.kind);

    // Step 3 — passive button-grab matching for ButtonPress.
    if !handled_core_via_grab
        && handle_grabs
        && event.kind == PointerEventKind::ButtonPress
        && let Some((grab, hit_window)) = try_match_passive_grab(state, xid_map, event)
    {
        log::trace!(
            "pointer_fanout: PASSIVE-GRAB match button={} grab_owner={:?} grab_window=0x{:x} mode={}",
            event.detail,
            grab.owner,
            grab.grab_window.0,
            grab.pointer_mode,
        );
        crate::core_loop::grab_debug::log(
            state,
            &format!(
                "  -> route=PASSIVE-GRAB match owner={:?} grab_window=0x{:x} pointer_mode={}",
                grab.owner, grab.grab_window.0, grab.pointer_mode
            ),
        );
        // Activate the passive grab atomically with the dispatch.
        // `owner_events=true` qualifies for natural delivery when hit
        // is the grab window, a descendant of it, OR owned by the
        // grab client — see the Step-2 active-grab comment for the
        // mate-panel regression that drove this ownership-aware check.
        let target_qualifies_for_natural = hit_window == grab.grab_window
            || state
                .resources
                .is_descendant_of(hit_window, grab.grab_window)
            || state.resources.window_owner(hit_window) == Some(grab.owner);
        let redirect_to_grab = !grab.owner_events || !target_qualifies_for_natural;
        if grab.pointer_mode == 0 {
            state.frozen_pointer_event = Some(event);
        }
        state.pointer_grab = Some((grab.owner, grab.grab_window));
        state.pointer_grab_is_passive = true;

        if redirect_to_grab {
            if let Some(grab_target) = client_target_id(state, grab.owner) {
                let extras = fanout_event_to_clients(state, &[grab_target], |buf, seq, order| {
                    encode_pointer_event(
                        buf,
                        order,
                        PointerEventKind::ButtonPress,
                        seq,
                        event.detail,
                        event.time,
                        grab.grab_window,
                        ResourceId(0), // passive grab activation: no propagation child
                        event,
                        event.event_x,
                        event.event_y,
                    );
                });
                merge_dropped(&mut dropped, extras);
            }
            handled_core_via_grab = true;
        }
    }

    // Step 4 — normal core propagation, only when no grab took ownership.
    //
    // For Crossing events we also run when top_level_id is None: the
    // producer (`update_pointer_window`) emits Leave/Enter chain events
    // with host_xid pointing at the KMS root container for the
    // ROOT_WINDOW endpoint, and that host_xid isn't in xid_map.
    let is_crossing = matches!(
        event.kind,
        PointerEventKind::EnterNotify | PointerEventKind::LeaveNotify
    );
    if !handled_core_via_grab && (top_level_id_opt.is_some() || is_crossing) {
        let mask_bit = pointer_mask_bit(event.kind, event.state);
        let (nested_id, event_x, event_y, core_targets, propagation_child) =
            pointer_propagation_target_by_id(state, target, target_x, target_y, mask_bit)
                .unwrap_or((target, target_x, target_y, Vec::new(), ResourceId(0)));

        if matches!(
            event.kind,
            PointerEventKind::ButtonPress | PointerEventKind::ButtonRelease
        ) {
            log::debug!(
                "pointer_fanout: kind={:?} button={} host_xid=0x{:x} top_level=0x{:x} target=0x{:x} \
                 propagation_window=0x{:x} child=0x{:x} core_targets={:?} root=({},{}) event_xy=({},{})",
                event.kind,
                event.detail,
                event.host_xid,
                top_level_id.0,
                target.0,
                nested_id.0,
                propagation_child.0,
                core_targets.iter().map(|c| c.0).collect::<Vec<_>>(),
                event.root_x,
                event.root_y,
                event_x,
                event_y,
            );
            crate::core_loop::grab_debug::log(
                state,
                &format!(
                    "  -> route=NORMAL-PROP nested=0x{:x} child=0x{:x} core_targets={:?}",
                    nested_id.0,
                    propagation_child.0,
                    core_targets.iter().map(|c| c.0).collect::<Vec<_>>(),
                ),
            );
        }

        let extras = fanout_event_to_clients(state, &core_targets, |buf, seq, order| {
            encode_pointer_event(
                buf,
                order,
                event.kind,
                seq,
                event.detail,
                event.time,
                nested_id,
                propagation_child,
                event,
                event_x,
                event_y,
            );
        });
        merge_dropped(&mut dropped, extras);
    }

    // ── XI2 fanout ──────────────────────────────────────────────────
    //
    // Skip on replay: XI2 was already fanned out on the original
    // libinput-driven invocation. Re-running here would deliver a second
    // XI2 ButtonPress for the same physical click and confuse GTK gesture
    // controllers (see is_replay rationale on the public fn).
    if is_replay {
        return dropped;
    }
    let Some(top_level_id) = top_level_id_opt else {
        log::debug!(
            "pointer_fanout: kind={:?} host_xid=0x{:x} not in xid_map — XI2 fanout skipped",
            event.kind,
            event.host_xid,
        );
        return dropped;
    };
    let xi2_evtype = xi2_evtype(event.kind);
    let xi2_raw_evtype = xi2_raw_evtype(event.kind);

    // For XI2 the "event window" is the hit target — XI2 doesn't
    // propagate up through the event mask the way core does. Use the
    // original (untranslated) coords relative to the hit target.
    let (mut event_x, mut event_y) = (target_x, target_y);
    let mut nested_id = target;
    let (mut xi2_targets, xi2_raw_targets) =
        compute_xi2_targets(state, target, top_level_id, xi2_evtype, xi2_raw_evtype);

    // Synchronous passive XI2 button grabs freeze the device event at
    // the grab owner until XIAllowEvents(ReplayDevice) replays it.
    // Without this filter GTK sees the press on the unfocused target
    // before muffin finishes focus activation, then never receives the
    // replay it expects.
    if handle_grabs
        && event.kind == PointerEventKind::ButtonPress
        && state.pointer_grab_is_passive
        && state.frozen_pointer_event.is_some()
        && let Some((grab_owner, _)) = state.pointer_grab
    {
        xi2_targets.retain(|cid| *cid == grab_owner);
    }

    // Active device-grab redirection for XI2 device events. When a client
    // holds an active pointer grab (XIGrabDevice — or an activated passive
    // grab), the grabbed device's XI2 button/motion events must funnel to
    // the grab owner, reported against the grab window, even when the
    // pointer has moved onto another client's window. This mirrors the
    // core Step-2 redirect; without it a window-move grab (muffin) stops
    // receiving XI_Motion / XI_ButtonRelease the moment the drag pulls the
    // pointer off the grab window, so the move never ends and the button
    // stays "held". Crossings keep natural delivery (same as the core
    // path); raw events bypass grabs and are left untouched.
    if handle_grabs
        && !matches!(
            event.kind,
            PointerEventKind::EnterNotify | PointerEventKind::LeaveNotify
        )
        && let Some((grab_window, grab_client, gx, gy, owner_events)) = active_grab_target(state)
    {
        // Same ownership-aware natural-delivery test as the Step-2
        // core path: `owner_events=true` keeps motion on whichever
        // GTK sub-window the cursor is over when that sub-window is
        // OWNED BY the grab client, even if it's a sibling top-level
        // (mate-panel menu items) rather than a descendant of the
        // grab window. No-op when owner_events=false.
        let target_qualifies_for_natural = target == grab_window
            || state.resources.is_descendant_of(target, grab_window)
            || state.resources.window_owner(target) == Some(grab_client);
        if !owner_events || !target_qualifies_for_natural {
            xi2_targets.clear();
            xi2_targets.push(grab_client);
            nested_id = grab_window;
            event_x = clamp_grab_coord(event.root_x, gx);
            event_y = clamp_grab_coord(event.root_y, gy);
        }
    }

    if matches!(
        event.kind,
        PointerEventKind::ButtonPress | PointerEventKind::ButtonRelease
    ) {
        log::debug!(
            "pointer_fanout XI2: kind={:?} button={} time={} target=0x{:x} top_level=0x{:x} \
             xi2_targets={:?} xi2_raw_targets={:?} root=({},{}) event_xy=({},{}) state=0x{:x}",
            event.kind,
            event.detail,
            event.time,
            target.0,
            top_level_id.0,
            xi2_targets.iter().map(|c| c.0).collect::<Vec<_>>(),
            xi2_raw_targets.iter().map(|c| c.0).collect::<Vec<_>>(),
            event.root_x,
            event.root_y,
            event_x,
            event_y,
            event.state,
        );
    }

    // XI2 raw events.
    if let Some(raw_evtype) = xi2_raw_evtype {
        let extras = fanout_event_to_clients(state, &xi2_raw_targets, |buf, seq, order| {
            x11::encode_xi2_raw_event(
                buf,
                order,
                seq,
                XI2_MAJOR_OPCODE,
                raw_evtype,
                XI2_MASTER_POINTER_DEVICE_ID,
                event.time,
                u32::from(event.detail),
                XI2_SLAVE_POINTER_DEVICE_ID,
                i32::from(event.root_x),
                i32::from(event.root_y),
            );
        });
        merge_dropped(&mut dropped, extras);
    }

    // If this is a wheel button press (4 = up, 5 = down, 6 = left,
    // 7 = right), prepend an XI_Motion event carrying the scroll-axis
    // valuator update before the XI_ButtonPress. The Motion's axis
    // value is the **CUMULATIVE** scroll counter, not the per-event
    // delta — XI2 §11.5: clients compute the delta as
    // `(current - previous) / increment`. The previous-value
    // baseline comes from `XIQueryDevice` (the valuator class
    // declares the current position) and `DeviceChanged` events as
    // clients connect mid-session.
    //
    // A pre-2026-05-29 yserver sent `delta` (±1) here. GDK reads the
    // axisvalue, subtracts its cached previous (which after the first
    // scroll is also 1), and gets 0 — no scroll. The first scroll
    // worked (1 - 0 = 1), every subsequent scroll on the same client
    // got stuck. This bug went unnoticed because GDK was falling back
    // to the legacy XI_ButtonPress(4..7) emulation (XIPointerEmulated
    // flag wasn't being set, so GDK accepted those buttons as scroll).
    // The XI_POINTER_EMULATED fix (Chrome scroll-crash repair) made
    // GDK correctly skip the emulated buttons, exposing the latent
    // cumulative-vs-delta bug.
    //
    // ButtonRelease doesn't carry an axis update; only ButtonPress does.
    let scroll_axis_info: Option<(u8, i32)> = if event.kind == PointerEventKind::ButtonPress
        && (event.detail >= 4 && event.detail <= 7)
    {
        let (axis_idx, delta): (usize, i32) = match event.detail {
            4 => (0, -1),
            5 => (0, 1),
            6 => (1, -1),
            7 => (1, 1),
            _ => unreachable!(),
        };
        state.scroll_axis_value[axis_idx] = state.scroll_axis_value[axis_idx].wrapping_add(delta);
        let scroll_axis_num: u8 = if axis_idx == 0 { 2 } else { 3 };
        Some((scroll_axis_num, state.scroll_axis_value[axis_idx]))
    } else {
        None
    };

    // XI2 device events (crossing or non-crossing).
    let extras = fanout_event_to_clients(state, &xi2_targets, |buf, seq, order| {
        if matches!(
            event.kind,
            PointerEventKind::EnterNotify | PointerEventKind::LeaveNotify
        ) {
            x11::encode_xi2_crossing_event(
                buf,
                order,
                seq,
                XI2_MAJOR_OPCODE,
                xi2_evtype,
                XI2_MASTER_POINTER_DEVICE_ID,
                event.time,
                ROOT_WINDOW,
                nested_id,
                event.root_x,
                event.root_y,
                event_x,
                event_y,
                event.state,
                0,
                0,
                XI2_SLAVE_POINTER_DEVICE_ID,
            );
        } else {
            if let Some((axis, value)) = scroll_axis_info {
                x11::encode_xi2_motion_with_scroll(
                    buf,
                    order,
                    seq,
                    XI2_MAJOR_OPCODE,
                    XI2_MASTER_POINTER_DEVICE_ID,
                    event.time,
                    ROOT_WINDOW,
                    nested_id,
                    event.root_x,
                    event.root_y,
                    event_x,
                    event_y,
                    event.state,
                    XI2_SLAVE_POINTER_DEVICE_ID,
                    axis,
                    value,
                );
            }
            // Mark scroll-emulated XI_ButtonPress/Release(4..7) with
            // XIPointerEmulated so XI2-aware clients discard the legacy
            // button event after consuming the matching XI_Motion
            // scroll-axis update. Skipping this flag double-dispatches
            // wheel input — release Chrome stack-smashed on rapid
            // scroll into yserver from this exact gap (see
            // `yserver-protocol::x11::XI_POINTER_EMULATED` for the full
            // rationale).
            let xi2_flags: u32 = if matches!(
                event.kind,
                PointerEventKind::ButtonPress | PointerEventKind::ButtonRelease
            ) && (4..=7).contains(&event.detail)
            {
                x11::XI_POINTER_EMULATED
            } else {
                0
            };
            x11::encode_xi2_device_event(
                buf,
                order,
                seq,
                XI2_MAJOR_OPCODE,
                xi2_evtype,
                XI2_MASTER_POINTER_DEVICE_ID,
                event.time,
                ROOT_WINDOW,
                nested_id,
                ResourceId(0), // XI2 doesn't propagate; event=hit-target, so child=None
                event.root_x,
                event.root_y,
                event_x,
                event_y,
                event.state,
                u32::from(event.detail),
                XI2_SLAVE_POINTER_DEVICE_ID,
                xi2_flags,
            );
        }
    });
    merge_dropped(&mut dropped, extras);

    dropped
}

fn translate_host_event(
    state: &ServerState,
    xid_map: &HostXidMap,
    event: HostPointerEvent,
) -> HostPointerEvent {
    let Some(top_level_id) = xid_map.get(&event.host_xid).copied() else {
        return event;
    };
    let Some((rx, ry)) = state
        .resources
        .window(top_level_id)
        .map(|w| (w.x + event.event_x, w.y + event.event_y))
    else {
        return event;
    };
    HostPointerEvent {
        root_x: rx,
        root_y: ry,
        ..event
    }
}

fn active_grab_target(
    state: &ServerState,
) -> Option<(yserver_protocol::x11::ResourceId, ClientId, i32, i32, bool)> {
    let (client_id, grab_window) = state.pointer_grab?;
    let target = client_target_id(state, client_id)?;
    let (gx, gy) = state.resources.window_absolute_position(grab_window);
    // `owner_events` from the active grab record. Passive button-grabs
    // (activated via try_match_passive_grab) do not populate
    // `active_pointer_grab`, so look up the matching passive grab and
    // preserve its `owner_events` flag; otherwise default to false
    // (X11 implicit grab semantics — events report against the grab
    // window).
    let owner_events = if state.pointer_grab_is_passive {
        state
            .button_grabs
            .iter()
            .rev()
            .find(|g| g.owner == client_id && g.grab_window == grab_window)
            .is_some_and(|g| g.owner_events)
    } else {
        state
            .active_pointer_grab
            .filter(|g| g.owner == client_id)
            .is_some_and(|g| g.owner_events)
    };
    Some((grab_window, target, gx, gy, owner_events))
}

fn release_passive_grab_on_button_release(state: &mut ServerState, kind: PointerEventKind) {
    if kind == PointerEventKind::ButtonRelease && state.pointer_grab_is_passive {
        state.pointer_grab = None;
        state.pointer_grab_is_passive = false;
        state.frozen_pointer_event = None;
    }
}

fn clamp_grab_coord(root_coord: i16, grab_origin: i32) -> i16 {
    i32::from(root_coord)
        .saturating_sub(grab_origin)
        .clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16
}

fn try_match_passive_grab(
    state: &ServerState,
    xid_map: &HostXidMap,
    event: HostPointerEvent,
) -> Option<(
    crate::server::PassiveButtonGrab,
    yserver_protocol::x11::ResourceId,
)> {
    let (hit_window, _, _) = state
        .root_pointer_target_at(event.root_x, event.root_y)
        .or_else(|| {
            let top_level_id = xid_map.get(&event.host_xid).copied()?;
            state
                .pointer_target_at(top_level_id, event.event_x, event.event_y)
                .or(Some((top_level_id, event.event_x, event.event_y)))
        })?;
    let grab = state.find_passive_grab(hit_window, event.detail, event.state)?;
    Some((grab, hit_window))
}

fn pointer_mask_bit(kind: PointerEventKind, state_mask: u16) -> u32 {
    match kind {
        PointerEventKind::ButtonPress => 0x0000_0004,
        PointerEventKind::ButtonRelease => 0x0000_0008,
        PointerEventKind::MotionNotify => {
            let mut bits: u32 = 0x0000_0040;
            let buttons_held = (state_mask >> 8) & 0x1f;
            if buttons_held != 0 {
                bits |= 0x0000_2000;
                for n in 0..5 {
                    if buttons_held & (1 << n) != 0 {
                        bits |= 0x0000_0100 << n;
                    }
                }
            }
            bits
        }
        PointerEventKind::EnterNotify => 0x0000_0010,
        PointerEventKind::LeaveNotify => 0x0000_0020,
    }
}

fn xi2_evtype(kind: PointerEventKind) -> u16 {
    match kind {
        PointerEventKind::ButtonPress => 4,
        PointerEventKind::ButtonRelease => 5,
        PointerEventKind::MotionNotify => 6,
        PointerEventKind::EnterNotify => 7,
        PointerEventKind::LeaveNotify => 8,
    }
}

fn xi2_raw_evtype(kind: PointerEventKind) -> Option<u16> {
    match kind {
        PointerEventKind::ButtonPress => Some(15),
        PointerEventKind::ButtonRelease => Some(16),
        PointerEventKind::MotionNotify => Some(17),
        PointerEventKind::EnterNotify | PointerEventKind::LeaveNotify => None,
    }
}

fn compute_xi2_targets(
    state: &ServerState,
    target: yserver_protocol::x11::ResourceId,
    top_level_id: yserver_protocol::x11::ResourceId,
    xi2_evtype: u16,
    xi2_raw_evtype: Option<u16>,
) -> (Vec<ClientId>, Vec<ClientId>) {
    let mut xi2_targets: Vec<ClientId> = Vec::new();
    let mut xi2_raw_targets: Vec<ClientId> = Vec::new();
    if xi2_evtype == 0 {
        return (xi2_targets, xi2_raw_targets);
    }
    for (cid_u32, c) in state.clients.iter() {
        let cid = ClientId(*cid_u32);
        let mask = xi2_mask_for_client(
            c,
            target,
            top_level_id,
            &[
                XI2_SLAVE_POINTER_DEVICE_ID,
                XI2_MASTER_POINTER_DEVICE_ID,
                1,
                0,
            ],
        );
        if mask & (1 << xi2_evtype) != 0 {
            xi2_targets.push(cid);
        }
        if let Some(raw_evtype) = xi2_raw_evtype {
            if mask & (1 << raw_evtype) != 0 {
                xi2_raw_targets.push(cid);
            }
            let root_mask = xi2_mask_for_client(
                c,
                ROOT_WINDOW,
                ROOT_WINDOW,
                &[
                    1,
                    0,
                    XI2_SLAVE_POINTER_DEVICE_ID,
                    XI2_MASTER_POINTER_DEVICE_ID,
                ],
            );
            if root_mask & (1 << raw_evtype) != 0 && !xi2_raw_targets.contains(&cid) {
                xi2_raw_targets.push(cid);
            }
        }
    }
    (xi2_targets, xi2_raw_targets)
}

#[allow(clippy::too_many_arguments)]
fn encode_pointer_event(
    buf: &mut Vec<u8>,
    order: yserver_protocol::x11::ClientByteOrder,
    kind: PointerEventKind,
    seq: SequenceNumber,
    detail: u8,
    time: u32,
    target_window: yserver_protocol::x11::ResourceId,
    child: yserver_protocol::x11::ResourceId,
    event: HostPointerEvent,
    event_x: i16,
    event_y: i16,
) {
    let pointer = x11::PointerEvent {
        sequence: seq,
        detail,
        time,
        root: ROOT_WINDOW,
        event: target_window,
        child,
        root_x: event.root_x,
        root_y: event.root_y,
        event_x,
        event_y,
        state: event.state,
    };
    match kind {
        PointerEventKind::ButtonPress => x11::encode_button_press_event(buf, order, pointer),
        PointerEventKind::ButtonRelease => x11::encode_button_release_event(buf, order, pointer),
        PointerEventKind::MotionNotify => x11::encode_motion_notify_event(
            buf,
            order,
            x11::PointerEvent {
                detail: 0,
                ..pointer
            },
        ),
        // For Crossing events, `child` and `detail` come from the
        // producer (HostPointerEvent), which has the spec-correct
        // values computed by `crossings::normal_mode_crossings` /
        // `implicit_grab_crossings`. The fanout-walk's
        // `propagation_child` is the right value for Button/Motion
        // (where it identifies the immediate descendant of the
        // propagation target on the path to the source), but NOT for
        // crossings — crossing `child` per X11 spec is per-event in
        // the chain (None on endpoints, the next inferior on virtual
        // intermediates) and the propagation walk can't know which is
        // which.
        PointerEventKind::EnterNotify => x11::encode_enter_notify_event(
            buf,
            order,
            x11::CrossingEvent {
                sequence: seq,
                time,
                root: ROOT_WINDOW,
                event: target_window,
                child: yserver_protocol::x11::ResourceId(event.child),
                root_x: event.root_x,
                root_y: event.root_y,
                event_x,
                event_y,
                state: event.state,
                detail: event.detail,
                mode: event.crossing_mode,
            },
        ),
        PointerEventKind::LeaveNotify => x11::encode_leave_notify_event(
            buf,
            order,
            x11::CrossingEvent {
                sequence: seq,
                time,
                root: ROOT_WINDOW,
                event: target_window,
                child: yserver_protocol::x11::ResourceId(event.child),
                root_x: event.root_x,
                root_y: event.root_y,
                event_x,
                event_y,
                state: event.state,
                detail: event.detail,
                mode: event.crossing_mode,
            },
        ),
    }
}

fn merge_dropped(into: &mut Vec<ClientId>, more: Vec<ClientId>) {
    for cid in more {
        if !into.contains(&cid) {
            into.push(cid);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::{ScreenSaverActive, ServerState};
    use yserver_protocol::x11::ClientId;

    use crate::server::ClientState;
    use std::{
        collections::{HashMap, HashSet, VecDeque},
        io::Read,
        os::unix::net::UnixStream,
        sync::{Arc, Mutex, atomic::AtomicU16},
    };

    // Duplicated from process_request.rs::tests. If you change one,
    // change both. A shared test_fixtures module is the right home
    // long-term; tracked as a follow-up.
    fn install_client(state: &mut ServerState, id: u32) -> UnixStream {
        use crate::resources::ROOT_WINDOW;
        use yserver_protocol::x11::ClientByteOrder;
        let (a, b) = UnixStream::pair().unwrap();
        state.clients.insert(
            id,
            ClientState {
                writer: Arc::new(Mutex::new(a)),
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: Arc::new(AtomicU16::new(0)),
                resource_id_base: 0,
                resource_id_mask: u32::MAX,
                event_masks: HashMap::new(),
                save_set: HashSet::new(),
                big_requests_enabled: false,
                xi2_masks: HashMap::new(),
                outbound: VecDeque::new(),
                watching_writable: false,
                focused_window: ROOT_WINDOW,
                reader_control: None,
            },
        );
        b
    }

    fn read_all_available(peer: &mut UnixStream) -> Vec<u8> {
        peer.set_nonblocking(true).expect("set_nonblocking");
        let mut out = Vec::new();
        let mut buf = [0u8; 512];
        loop {
            match peer.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(err) => panic!("read failed: {err}"),
            }
        }
        peer.set_nonblocking(false).expect("unset_nonblocking");
        out
    }

    fn motion_event() -> HostPointerEvent {
        HostPointerEvent {
            kind: PointerEventKind::MotionNotify,
            host_xid: 0,
            detail: 0,
            time: 1,
            root_x: 10,
            root_y: 20,
            event_x: 10,
            event_y: 20,
            state: 0,
            crossing_mode: 0,
            child: 0,
        }
    }

    #[test]
    fn pointer_event_resets_dpms_last_activity() {
        use std::time::{Duration, Instant};
        let mut state = ServerState::new();
        state.dpms.kms_capable = true;
        state.dpms.enabled = true;
        state.dpms.last_activity = Instant::now() - Duration::from_secs(10);
        let stale = state.dpms.last_activity;
        let xid_map = HostXidMap::new();
        let mut backend = crate::backend::recording::RecordingBackend::default();

        let _ = pointer_event_fanout_to_state(
            &mut state,
            &mut backend,
            &xid_map,
            motion_event(),
            true,
            false,
        );

        let elapsed = state.dpms.last_activity.duration_since(stale);
        assert!(
            elapsed > Duration::from_secs(9),
            "last_activity should be ≈now, not stale"
        );
    }

    #[test]
    fn pointer_event_during_off_wakes_via_set_dpms_power_on() {
        let mut state = ServerState::new();
        state.dpms.kms_capable = true;
        state.dpms.enabled = true;
        state.dpms.power_level = 3; // Off
        let xid_map = HostXidMap::new();
        let mut backend = crate::backend::recording::RecordingBackend::default();

        let _ = pointer_event_fanout_to_state(
            &mut state,
            &mut backend,
            &xid_map,
            motion_event(),
            true,
            false,
        );

        let calls = backend.calls.lock().unwrap().clone();
        assert!(
            calls
                .iter()
                .any(|c| matches!(c, crate::backend::recording::RecordedCall::SetDpmsPower(0))),
            "wake must call set_dpms_power(0); got {calls:?}"
        );
        assert_eq!(
            state.dpms.power_level, 0,
            "in-memory level should be On after wake"
        );
    }

    #[test]
    fn pointer_event_during_off_with_backend_error_still_advances_state() {
        let mut state = ServerState::new();
        state.dpms.kms_capable = true;
        state.dpms.enabled = true;
        state.dpms.power_level = 3;
        let xid_map = HostXidMap::new();
        let mut backend = crate::backend::recording::RecordingBackend::default();
        backend.dpms_set_returns_err = true;

        let _ = pointer_event_fanout_to_state(
            &mut state,
            &mut backend,
            &xid_map,
            motion_event(),
            true,
            false,
        );

        assert_eq!(
            state.dpms.power_level, 0,
            "state must advance on backend error"
        );
    }

    #[test]
    fn pointer_event_during_screen_saver_on_flips_off_via_independent_path() {
        // Standalone SS-On (DPMS still On). Motion event must flip
        // SS Off with forced=0.
        let mut state = ServerState::new();
        state.dpms.kms_capable = true;
        state.dpms.enabled = true;
        // dpms.power_level already 0 from new()
        state.screensaver.active = ScreenSaverActive::On;
        state.screensaver.selected_by.insert(ClientId(1), 0x01);
        let xid_map = HostXidMap::new();
        let mut backend = crate::backend::recording::RecordingBackend::default();

        let _ = pointer_event_fanout_to_state(
            &mut state,
            &mut backend,
            &xid_map,
            motion_event(),
            true,
            false,
        );

        assert_eq!(state.screensaver.active, ScreenSaverActive::Off);
        assert!(!state.screensaver.forced, "input-driven Off is non-forced");
    }

    #[test]
    fn pointer_event_updates_global_and_per_device_vcp_last_activity() {
        use std::time::Duration;
        let mut state = ServerState::new();
        state.dpms.last_activity = std::time::Instant::now() - Duration::from_secs(30);
        let stale = state.dpms.last_activity;
        let xid_map = HostXidMap::new();
        let mut backend = crate::backend::recording::RecordingBackend::default();

        let _ = pointer_event_fanout_to_state(
            &mut state,
            &mut backend,
            &xid_map,
            motion_event(),
            true,
            false,
        );

        assert!(
            state.dpms.last_activity > stale,
            "global last_activity advanced"
        );
        let vcp = state
            .per_device_last_activity
            .get(&2)
            .copied()
            .expect("VCP per-device entry inserted");
        assert!(vcp > stale, "VCP per-device last_activity advanced");
    }

    #[test]
    fn pointer_event_fires_neg_transition_alarm_when_prior_idle_crosses_threshold() {
        use std::time::Duration;
        use yserver_protocol::x11::sync as x11sync;
        let mut state = ServerState::new();
        // User idle for 90s, NegativeTransition alarm at 60s.
        state.dpms.last_activity = std::time::Instant::now() - Duration::from_secs(90);
        state
            .per_device_last_activity
            .insert(2, std::time::Instant::now() - Duration::from_secs(90));
        let alarm_id = 0x4000;
        state.sync_alarms.insert(
            alarm_id,
            crate::server::SyncAlarm {
                owner: ClientId(1),
                counter: x11sync::IDLETIME_DEVICE_VCP,
                wait_value: 60_000,
                delta: 0,
                test_type: x11sync::TEST_NEGATIVE_TRANSITION as u8,
                events: false,
                state: x11sync::ALARM_STATE_ACTIVE,
            },
        );
        let xid_map = HostXidMap::new();
        let mut backend = crate::backend::recording::RecordingBackend::default();

        let _ = pointer_event_fanout_to_state(
            &mut state,
            &mut backend,
            &xid_map,
            motion_event(),
            true,
            false,
        );

        // Alarm stays Active (Transition + delta=0 — Task 2 fix); cache reflects post-wake idle=0.
        assert_eq!(
            state.sync_alarms[&alarm_id].state,
            x11sync::ALARM_STATE_ACTIVE
        );
        assert_eq!(
            state
                .idletime_last_evaluated
                .get(&x11sync::IDLETIME_DEVICE_VCP)
                .copied(),
            Some(0),
            "post-wake last_evaluated should be 0"
        );
    }

    #[test]
    fn pointer_event_fires_neg_transition_alarm_on_per_device_idletime_vcp() {
        use std::time::Duration;
        use yserver_protocol::x11::sync as x11sync;
        let mut state = ServerState::new();
        let mut peer = install_client(&mut state, 1);
        state.dpms.last_activity = std::time::Instant::now() - Duration::from_secs(90);
        state
            .per_device_last_activity
            .insert(2, std::time::Instant::now() - Duration::from_secs(90));
        let alarm_id = 0x5000;
        state.sync_alarms.insert(
            alarm_id,
            crate::server::SyncAlarm {
                owner: ClientId(1),
                counter: x11sync::IDLETIME_DEVICE_VCP,
                wait_value: 60_000,
                delta: 0,
                test_type: x11sync::TEST_NEGATIVE_TRANSITION as u8,
                events: true, // load-bearing
                state: x11sync::ALARM_STATE_ACTIVE,
            },
        );
        let xid_map = HostXidMap::new();
        let mut backend = crate::backend::recording::RecordingBackend::default();

        let _ = pointer_event_fanout_to_state(
            &mut state,
            &mut backend,
            &xid_map,
            motion_event(),
            true,
            false,
        );

        // PRIMARY: AlarmNotify event type 84.
        let bytes = read_all_available(&mut peer);
        // AlarmNotify is a 32-byte sequential event; type byte at offset 0.
        assert!(
            bytes.len() >= 32,
            "expected AlarmNotify event (32B); got {} bytes",
            bytes.len()
        );
        assert_eq!(
            bytes[0], 84,
            "AlarmNotify event type (SYNC_FIRST_EVENT + 1)"
        );
        assert_eq!(bytes[1], 1, "AlarmNotify kind = AlarmNotify (1)");
        assert_eq!(
            state.sync_alarms[&alarm_id].state,
            x11sync::ALARM_STATE_ACTIVE
        );
        assert_eq!(
            state
                .idletime_last_evaluated
                .get(&x11sync::IDLETIME_DEVICE_VCP)
                .copied(),
            Some(0)
        );
    }
}
