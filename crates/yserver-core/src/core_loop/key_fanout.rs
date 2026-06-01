//! State-borrowing key event fanout.
//!
//! KeyPress / KeyRelease delivery follows the X11 keyboard model: at
//! most one window receives the event (the active grab's window if a
//! grab is in effect, otherwise the focused window). The event is
//! emitted to subscribers with `KeyPressMask` / `KeyReleaseMask` on
//! that window. XI2 device-event subscribers on the same window also
//! receive a parallel XI2 KeyPress / KeyRelease.
//!
//! "Focus" comes from the per-client `ClientState::focused_window`.
//! In practice all clients share the same value (every `SetInputFocus`
//! mirrors it across clients), so the helper picks the first non-ROOT
//! focus it sees. When every client is rooted, the event is dropped.

use yserver_protocol::x11::{self, ClientId, ResourceId};

use crate::{
    core_loop::fanout::{emit_window_event_to_state, fanout_event_to_clients},
    host_x11::HostKeyEvent,
    resources::ROOT_WINDOW,
    server::{ActiveKeyboardGrab, ActiveKeyboardGrabSource, ServerState, xi2_mask_for_client},
};

const KEY_PRESS_MASK: u32 = 0x0000_0001;
const KEY_RELEASE_MASK: u32 = 0x0000_0002;
const XI2_MAJOR_OPCODE: u8 = 137;
const XI2_KEYPRESS_EVTYPE: u16 = 2;
const XI2_KEYRELEASE_EVTYPE: u16 = 3;
const XI2_MASTER_KEYBOARD_DEVICE_ID: u16 = 3;
const XI2_SLAVE_KEYBOARD_DEVICE_ID: u16 = 5;
/// Modifier bits delivered on the wire (Shift|Lock|Control|Mod1|Mod4 = 0x004d).
const WIRE_MODIFIER_MASK: u16 = 0x004d;

/// Fan a host key event out to nested clients.
///
/// Returns the deduped list of clients whose outbound buffer overflowed
/// during the fanout — the caller (run_core) issues
/// `Message::ClientDisconnected` for each.
pub fn key_event_fanout_to_state(
    state: &mut ServerState,
    backend: &mut dyn crate::backend::Backend,
    event: HostKeyEvent,
) -> Vec<ClientId> {
    // DPMS: any key resets the idle timer; from any non-On level
    // we wake the screen *before* fanning out, so the first event
    // of the resumed session lands on a visible scanout.
    let now = std::time::Instant::now();
    // Capture priors BEFORE mutating; needed by the IDLETIME wake handler.
    #[allow(clippy::cast_possible_truncation)]
    let prior_global = now
        .duration_since(state.dpms.last_activity)
        .as_millis()
        .min(u128::from(u32::MAX)) as i64;
    // XI2 master device IDs are always small (3 here); cast u16 → u8 is safe.
    // Per-device prior: fall back to global if no per-device entry yet.
    // Matches `idletime_baseline`'s fallback (server.rs Task 1) — without
    // this, the very first input event for a device whose baseline isn't
    // recorded would compute prior_device=0 and a per-device Negative
    // alarm (whose wait_value > 0) would not see the `old > wait` half of
    // its trigger.
    let prior_device = state
        .per_device_last_activity
        .get(&(XI2_MASTER_KEYBOARD_DEVICE_ID as u8))
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
        .insert(XI2_MASTER_KEYBOARD_DEVICE_ID as u8, now);

    // IDLETIME wake: fires Negative-* alarms before the input event itself
    // reaches clients (predictable ordering).
    crate::core_loop::process_request::evaluate_idletime_negative_alarms_on_input_wake(
        state,
        XI2_MASTER_KEYBOARD_DEVICE_ID as u8,
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

    let route = key_route(state, &event);
    crate::core_loop::grab_debug::log(
        state,
        &format!(
            "KEY {} keycode={} state=0x{:x} route={}",
            if event.pressed { "press" } else { "release" },
            event.keycode,
            event.state,
            match &route {
                KeyRoute::Drop => "DROP".to_string(),
                KeyRoute::PassiveGrabOwner { grab_window, .. } =>
                    format!("PASSIVE-GRAB grab_window=0x{:x}", grab_window.0),
                KeyRoute::Window(w) => format!("WINDOW 0x{:x}", w.0),
            },
        ),
    );
    match route {
        KeyRoute::Drop => Vec::new(),
        KeyRoute::PassiveGrabOwner {
            owner,
            grab_window,
            freeze,
        } => {
            // Synchronous passive key grab: hold the activating press
            // so AllowEvents(ReplayKeyboard) can replay it to the
            // focus window if the grab owner declines it. Mirrors the
            // sync passive-button-grab freeze in `pointer_fanout`.
            if freeze && event.pressed {
                state.frozen_keyboard_event = Some(event);
            }
            deliver_key_to_grab_owner(state, &event, owner, grab_window)
        }
        KeyRoute::Window(window) => deliver_key_to_window(state, &event, window),
    }
}

/// Replay a frozen key (held by a synchronous passive grab) to the
/// current focus window, bypassing grab matching. Called from the
/// AllowEvents `ReplayKeyboard` / XIAllowEvents `ReplayDevice` path
/// after the grab owner declines the key. Mirrors Xorg
/// `ComputeFreezes` → `DeliverFocusedEvent` (dix/events.c:1360).
pub fn replay_frozen_key_to_focus(state: &mut ServerState, event: HostKeyEvent) -> Vec<ClientId> {
    let focus = current_focus(state);
    if focus == ROOT_WINDOW {
        return Vec::new();
    }
    deliver_key_to_window(state, &event, focus)
}

/// Deliver a key event to a single window's subscribers — the normal
/// path (focus window, or an explicit-grab window). Core KeyPress/
/// KeyRelease to `KeyPressMask`/`KeyReleaseMask` subscribers, plus a
/// parallel XI2 device event to XI2 selectors on the same window.
fn deliver_key_to_window(
    state: &mut ServerState,
    event: &HostKeyEvent,
    target_window: ResourceId,
) -> Vec<ClientId> {
    let mask_bit = if event.pressed {
        KEY_PRESS_MASK
    } else {
        KEY_RELEASE_MASK
    };
    let mut dropped =
        emit_window_event_to_state(state, target_window, mask_bit, |buf, seq, order| {
            x11::encode_key_event(buf, order, key_event_wire(event, seq, target_window));
        });

    let xi2_evtype = xi2_evtype_for(event);
    let xi2_targets: Vec<ClientId> = state
        .clients
        .iter()
        .filter_map(|(id, client)| {
            let mask = xi2_mask_for_client(
                client,
                target_window,
                target_window,
                &[
                    XI2_SLAVE_KEYBOARD_DEVICE_ID,
                    XI2_MASTER_KEYBOARD_DEVICE_ID,
                    1,
                    0,
                ],
            );
            if mask & (1 << xi2_evtype) != 0 {
                Some(ClientId(*id))
            } else {
                None
            }
        })
        .collect();
    if !xi2_targets.is_empty() {
        let xi2_dropped = fanout_event_to_clients(state, &xi2_targets, |buf, seq, order| {
            encode_key_xi2(buf, order, seq, event, target_window);
        });
        merge_dropped(&mut dropped, xi2_dropped);
    }
    dropped
}

/// Deliver a key event to the grab owner client, addressed to the
/// grab window. When a passive (or explicit) keyboard grab is active,
/// X11 delivers to the *grab owner* using the grab's event mask, not
/// to whichever clients happen to have selected key events on the
/// grab window. yserver previously delivered via window selection, so
/// a grab owner that registered the grab via `XIPassiveGrabDevice`
/// (without a matching `XISelectEvents` on the root) received nothing
/// and the key was lost. Both core and XI2 forms are sent (the owner
/// uses whichever protocol it grabbed with), mirroring the passive
/// button-grab delivery in `pointer_fanout`.
fn deliver_key_to_grab_owner(
    state: &mut ServerState,
    event: &HostKeyEvent,
    owner: ClientId,
    grab_window: ResourceId,
) -> Vec<ClientId> {
    let mut dropped = fanout_event_to_clients(state, &[owner], |buf, seq, order| {
        x11::encode_key_event(buf, order, key_event_wire(event, seq, grab_window));
    });
    let xi2_dropped = fanout_event_to_clients(state, &[owner], |buf, seq, order| {
        encode_key_xi2(buf, order, seq, event, grab_window);
    });
    merge_dropped(&mut dropped, xi2_dropped);
    dropped
}

fn xi2_evtype_for(event: &HostKeyEvent) -> u16 {
    if event.pressed {
        XI2_KEYPRESS_EVTYPE
    } else {
        XI2_KEYRELEASE_EVTYPE
    }
}

fn key_event_wire(
    event: &HostKeyEvent,
    sequence: x11::SequenceNumber,
    target_window: ResourceId,
) -> x11::KeyEvent {
    x11::KeyEvent {
        pressed: event.pressed,
        keycode: event.keycode,
        sequence,
        time: event.time,
        root: ROOT_WINDOW,
        event: target_window,
        root_x: event.root_x,
        root_y: event.root_y,
        event_x: event.event_x,
        event_y: event.event_y,
        state: event.state & WIRE_MODIFIER_MASK,
    }
}

fn encode_key_xi2(
    buf: &mut Vec<u8>,
    order: x11::ClientByteOrder,
    seq: x11::SequenceNumber,
    event: &HostKeyEvent,
    target_window: ResourceId,
) {
    x11::encode_xi2_device_event(
        buf,
        order,
        seq,
        XI2_MAJOR_OPCODE,
        xi2_evtype_for(event),
        XI2_MASTER_KEYBOARD_DEVICE_ID,
        event.time,
        ROOT_WINDOW,
        target_window,
        ResourceId(0), // child=None; key events target the window directly
        event.root_x,
        event.root_y,
        event.event_x,
        event.event_y,
        event.state & WIRE_MODIFIER_MASK,
        u32::from(event.keycode),
        XI2_SLAVE_KEYBOARD_DEVICE_ID,
        0, // flags: no XIPointerEmulated on key events
    );
}

fn merge_dropped(into: &mut Vec<ClientId>, more: Vec<ClientId>) {
    for cid in more {
        if !into.contains(&cid) {
            into.push(cid);
        }
    }
}

/// Where a key event should go.
enum KeyRoute {
    /// No focus and no grab — drop the event.
    Drop,
    /// A passive key grab is active — deliver only to the grab owner.
    /// `freeze` is set for a synchronous grab on the activating press,
    /// signalling that the event must be held for possible replay.
    PassiveGrabOwner {
        owner: ClientId,
        grab_window: ResourceId,
        freeze: bool,
    },
    /// Normal delivery to a window's subscribers (focus window, or an
    /// explicit-grab window).
    Window(ResourceId),
}

/// Apply X11 keyboard routing rules. May activate a passive grab or
/// auto-release one on the matching key release.
fn key_route(state: &mut ServerState, event: &HostKeyEvent) -> KeyRoute {
    // Active grab in effect.
    if let Some(g) = state.active_keyboard_grab {
        let passive = matches!(g.source, ActiveKeyboardGrabSource::PassiveKey { .. });
        // Auto-release a passive-key grab on the matching key-release
        // (the release still goes to the grab owner below).
        if !event.pressed
            && let ActiveKeyboardGrabSource::PassiveKey { keycode: kc } = g.source
            && kc == event.keycode
        {
            state.active_keyboard_grab = None;
            state.frozen_keyboard_event = None;
        }
        if passive {
            return KeyRoute::PassiveGrabOwner {
                owner: g.owner,
                grab_window: g.grab_window,
                freeze: false,
            };
        }
        // Explicit grab (GrabKeyboard): keep existing window delivery.
        return KeyRoute::Window(g.grab_window);
    }

    let focus = current_focus(state);

    // Press: try to match a passive key grab, activating it.
    if event.pressed
        && let Some((owner, grab_window, keyboard_mode)) = state
            .find_key_grab(focus, event.keycode, event.state)
            .map(|g| (g.owner, g.grab_window, g.keyboard_mode))
    {
        state.active_keyboard_grab = Some(ActiveKeyboardGrab {
            owner,
            grab_window,
            source: ActiveKeyboardGrabSource::PassiveKey {
                keycode: event.keycode,
            },
        });
        return KeyRoute::PassiveGrabOwner {
            owner,
            grab_window,
            // keyboard_mode 0 == Synchronous → freeze for replay.
            freeze: keyboard_mode == 0,
        };
    }

    // Drop key events that would land on root with no grab.
    if focus == ROOT_WINDOW {
        return KeyRoute::Drop;
    }
    KeyRoute::Window(focus)
}

/// Pick the current keyboard focus.
///
/// Per-client `focused_window` is intended to be a global value
/// mirrored across clients. Pick the first non-ROOT focus we see; if
/// every client is rooted, return `ROOT_WINDOW`.
pub(crate) fn current_focus(state: &ServerState) -> ResourceId {
    state
        .clients
        .values()
        .map(|c| c.focused_window)
        .find(|f| *f != ROOT_WINDOW)
        .unwrap_or(ROOT_WINDOW)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::{
        ActiveKeyboardGrab, ActiveKeyboardGrabSource, KeyGrab, ScreenSaverActive, ServerState,
    };
    use yserver_protocol::x11::ClientId;

    use crate::server::ClientState;
    use std::{
        collections::{HashMap, HashSet, VecDeque},
        io::Read,
        os::unix::net::UnixStream,
        sync::{Arc, Mutex, atomic::AtomicU16},
    };
    use yserver_protocol::x11::ClientByteOrder;

    // Duplicated from process_request.rs::tests. If you change one,
    // change both. A shared test_fixtures module is the right home
    // long-term; tracked as a follow-up.
    fn install_client(state: &mut ServerState, id: u32) -> UnixStream {
        use crate::resources::ROOT_WINDOW;
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

    fn key_event(pressed: bool, keycode: u8) -> HostKeyEvent {
        HostKeyEvent {
            pressed,
            keycode,
            time: 1,
            root_x: 10,
            root_y: 20,
            event_x: 10,
            event_y: 20,
            state: 0,
        }
    }

    /// Install a client whose core+XI2 selection on `window` is
    /// `core_mask` / `xi2_mask`. Returns the peer socket for reading
    /// what the server delivered.
    fn install_kf(
        state: &mut ServerState,
        id: u32,
        window: ResourceId,
        core_mask: u32,
        xi2_mask: u32,
    ) -> UnixStream {
        let (server_side, peer) = UnixStream::pair().unwrap();
        let client = ClientState {
            writer: Arc::new(Mutex::new(server_side)),
            byte_order: ClientByteOrder::LittleEndian,
            last_sequence: Arc::new(AtomicU16::new(0)),
            resource_id_base: 0,
            resource_id_mask: 0,
            event_masks: HashMap::from([(window, core_mask)]),
            save_set: HashSet::new(),
            big_requests_enabled: false,
            xi2_masks: HashMap::from([((window, 3u16), xi2_mask)]),
            outbound: VecDeque::new(),
            watching_writable: false,
            focused_window: ROOT_WINDOW,
            reader_control: None,
        };
        state.clients.insert(id, client);
        peer
    }

    fn received_bytes(peer: &mut UnixStream) -> usize {
        peer.set_nonblocking(true).unwrap();
        let mut buf = [0u8; 512];
        peer.read(&mut buf).unwrap_or(0)
    }

    /// A synchronous passive key grab routes the activating press to
    /// the grab *owner* (even though the owner has no per-window key
    /// selection, only the grab), and freezes the event for replay.
    /// This is the dead-`p`-in-wezterm fix: previously the press was
    /// delivered via window selection on the grab window, so a grab
    /// owner that registered via XIPassiveGrabDevice received nothing.
    #[test]
    fn sync_passive_key_grab_delivers_to_owner_and_freezes() {
        let mut state = ServerState::new();
        // Grab owner selects NOTHING (mask 0) — only the grab matters.
        let mut owner = install_kf(&mut state, 7, ROOT_WINDOW, 0, 0);
        let mut backend = crate::backend::recording::RecordingBackend::default();
        state.key_grabs.push(KeyGrab {
            owner: ClientId(7),
            grab_window: ROOT_WINDOW,
            keycode: 33,
            modifiers: 0,
            owner_events: false,
            pointer_mode: 1,
            keyboard_mode: 0, // synchronous → freeze
        });

        let dropped = key_event_fanout_to_state(&mut state, &mut backend, key_event(true, 33));
        assert!(dropped.is_empty());
        assert!(
            matches!(
                state.active_keyboard_grab,
                Some(ActiveKeyboardGrab {
                    owner: ClientId(7),
                    source: ActiveKeyboardGrabSource::PassiveKey { keycode: 33 },
                    ..
                })
            ),
            "passive grab must activate, owned by client 7"
        );
        assert!(
            state.frozen_keyboard_event.is_some(),
            "synchronous grab must freeze the press for replay"
        );
        assert!(
            received_bytes(&mut owner) > 0,
            "grab owner must receive the key press despite no window selection"
        );
    }

    /// `replay_frozen_key_to_focus` re-delivers the held key to the
    /// focused window's subscribers, bypassing the grab — the path
    /// AllowEvents(ReplayKeyboard) drives so the focused app (wezterm)
    /// finally sees the key the WM declined.
    #[test]
    fn replay_frozen_key_reaches_focus_window() {
        const FOCUS_WIN: u32 = 0x0020_0007;
        let mut state = ServerState::new();
        // Focused client selects core KeyPress on its window.
        let mut focus_peer = install_kf(&mut state, 9, ResourceId(FOCUS_WIN), KEY_PRESS_MASK, 0);
        state.clients.get_mut(&9).unwrap().focused_window = ResourceId(FOCUS_WIN);

        let _ = replay_frozen_key_to_focus(&mut state, key_event(true, 33));
        assert!(
            received_bytes(&mut focus_peer) > 0,
            "replayed key must reach the focused window's subscriber"
        );
    }

    /// Asynchronous passive key grab (keyboard_mode=1) does NOT freeze:
    /// the owner gets the press but there's nothing to replay.
    #[test]
    fn async_passive_key_grab_does_not_freeze() {
        let mut state = ServerState::new();
        let _owner = install_kf(&mut state, 7, ROOT_WINDOW, 0, 0);
        let mut backend = crate::backend::recording::RecordingBackend::default();
        state.key_grabs.push(KeyGrab {
            owner: ClientId(7),
            grab_window: ROOT_WINDOW,
            keycode: 33,
            modifiers: 0,
            owner_events: false,
            pointer_mode: 1,
            keyboard_mode: 1, // asynchronous → no freeze
        });
        let _ = key_event_fanout_to_state(&mut state, &mut backend, key_event(true, 33));
        assert!(state.active_keyboard_grab.is_some());
        assert!(
            state.frozen_keyboard_event.is_none(),
            "async grab must not freeze"
        );
    }

    #[test]
    fn dropped_when_focus_is_root_and_no_grabs() {
        // No clients = no focus = nothing to fan out. Just verify the
        // helper returns an empty drop list cleanly.
        let mut state = ServerState::new();
        let mut backend = crate::backend::recording::RecordingBackend::default();
        let dropped = key_event_fanout_to_state(&mut state, &mut backend, key_event(true, 38));
        assert!(dropped.is_empty());
    }

    #[test]
    fn passive_key_grab_activates_on_press_and_clears_on_release() {
        let mut state = ServerState::new();
        // No focus is set on any client — find_key_grab walks up from
        // focus. Setting the grab on ROOT exercises the matching path.
        let grab_owner = ClientId(7);
        let mut backend = crate::backend::recording::RecordingBackend::default();
        state.key_grabs.push(KeyGrab {
            owner: grab_owner,
            grab_window: ROOT_WINDOW,
            keycode: 38,
            modifiers: 0,
            owner_events: false,
            pointer_mode: 1,
            keyboard_mode: 1,
        });
        // Press: activates passive grab.
        let _ = key_event_fanout_to_state(&mut state, &mut backend, key_event(true, 38));
        match state.active_keyboard_grab {
            Some(ActiveKeyboardGrab {
                owner,
                grab_window,
                source: ActiveKeyboardGrabSource::PassiveKey { keycode },
            }) => {
                assert_eq!(owner, grab_owner);
                assert_eq!(grab_window, ROOT_WINDOW);
                assert_eq!(keycode, 38);
            }
            other => panic!("expected PassiveKey grab, got {other:?}"),
        }
        // Release with matching keycode clears the grab.
        let _ = key_event_fanout_to_state(&mut state, &mut backend, key_event(false, 38));
        assert!(state.active_keyboard_grab.is_none());
    }

    #[test]
    fn explicit_grab_persists_across_release() {
        let mut state = ServerState::new();
        let mut backend = crate::backend::recording::RecordingBackend::default();
        state.active_keyboard_grab = Some(ActiveKeyboardGrab {
            owner: ClientId(3),
            grab_window: ResourceId(0x100),
            source: ActiveKeyboardGrabSource::Explicit,
        });
        let _ = key_event_fanout_to_state(&mut state, &mut backend, key_event(false, 38));
        // Explicit grab is NOT cleared by a key release (only passive
        // grabs auto-clear). Persists until UngrabKeyboard.
        assert!(matches!(
            state.active_keyboard_grab,
            Some(ActiveKeyboardGrab {
                source: ActiveKeyboardGrabSource::Explicit,
                ..
            })
        ));
    }

    #[test]
    fn key_event_resets_dpms_last_activity() {
        use std::time::{Duration, Instant};
        let mut state = ServerState::new();
        state.dpms.kms_capable = true;
        state.dpms.enabled = true;
        state.dpms.last_activity = Instant::now() - Duration::from_secs(10);
        let stale = state.dpms.last_activity;
        let mut backend = crate::backend::recording::RecordingBackend::default();

        let _ = key_event_fanout_to_state(&mut state, &mut backend, key_event(true, 33));

        let elapsed = state.dpms.last_activity.duration_since(stale);
        assert!(
            elapsed > Duration::from_secs(9),
            "last_activity should be ≈now, not stale"
        );
    }

    #[test]
    fn key_event_during_off_wakes_via_set_dpms_power_on() {
        let mut state = ServerState::new();
        state.dpms.kms_capable = true;
        state.dpms.enabled = true;
        state.dpms.power_level = 3; // Off
        let mut backend = crate::backend::recording::RecordingBackend::default();

        let _ = key_event_fanout_to_state(&mut state, &mut backend, key_event(true, 33));

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
    fn key_event_during_off_with_backend_error_still_advances_state() {
        let mut state = ServerState::new();
        state.dpms.kms_capable = true;
        state.dpms.enabled = true;
        state.dpms.power_level = 3;
        let mut backend = crate::backend::recording::RecordingBackend::default();
        backend.dpms_set_returns_err = true;

        let _ = key_event_fanout_to_state(&mut state, &mut backend, key_event(true, 33));

        assert_eq!(
            state.dpms.power_level, 0,
            "state must advance on backend error"
        );
    }

    #[test]
    fn key_event_during_screen_saver_on_flips_off_via_independent_path() {
        // Pre-state: DPMS On (so the existing DPMS-wake prologue
        // doesn't fire), SS On (activated standalone via idle timer
        // or ForceScreenSaver). Input must flip SS Off with forced=0.
        let mut state = ServerState::new();
        state.dpms.kms_capable = true;
        state.dpms.enabled = true;
        // dpms.power_level already 0 from new()
        state.screensaver.active = ScreenSaverActive::On;
        state.screensaver.selected_by.insert(ClientId(1), 0x01);
        let mut backend = crate::backend::recording::RecordingBackend::default();

        let _ = key_event_fanout_to_state(&mut state, &mut backend, key_event(true, 33));

        assert_eq!(state.screensaver.active, ScreenSaverActive::Off);
        assert!(!state.screensaver.forced, "input-driven Off is non-forced");
    }

    #[test]
    fn key_event_updates_global_and_per_device_vck_last_activity() {
        use std::time::Duration;
        let mut state = ServerState::new();
        state.dpms.last_activity = std::time::Instant::now() - Duration::from_secs(30);
        let stale = state.dpms.last_activity;
        let mut backend = crate::backend::recording::RecordingBackend::default();

        let _ = key_event_fanout_to_state(&mut state, &mut backend, key_event(true, 33));

        assert!(
            state.dpms.last_activity > stale,
            "global last_activity advanced"
        );
        let vck = state
            .per_device_last_activity
            .get(&3)
            .copied()
            .expect("VCK per-device entry inserted");
        assert!(vck > stale, "VCK per-device last_activity advanced");
    }

    #[test]
    fn key_event_fires_neg_transition_alarm_when_prior_idle_crosses_threshold() {
        use std::time::Duration;
        use yserver_protocol::x11::sync as x11sync;
        let mut state = ServerState::new();
        // User idle for 90s, NegativeTransition alarm at 60s.
        state.dpms.last_activity = std::time::Instant::now() - Duration::from_secs(90);
        state
            .per_device_last_activity
            .insert(3, std::time::Instant::now() - Duration::from_secs(90));
        let alarm_id = 0x2000;
        state.sync_alarms.insert(
            alarm_id,
            crate::server::SyncAlarm {
                owner: ClientId(1),
                counter: x11sync::IDLETIME_COUNTER,
                wait_value: 60_000,
                delta: 0,
                test_type: x11sync::TEST_NEGATIVE_TRANSITION as u8,
                events: false,
                state: x11sync::ALARM_STATE_ACTIVE,
            },
        );
        let mut backend = crate::backend::recording::RecordingBackend::default();

        let _ = key_event_fanout_to_state(&mut state, &mut backend, key_event(true, 33));

        // Alarm stays Active (Transition + delta=0 — Task 2 fix); cache reflects post-wake idle=0.
        assert_eq!(
            state.sync_alarms[&alarm_id].state,
            x11sync::ALARM_STATE_ACTIVE
        );
        assert_eq!(
            state
                .idletime_last_evaluated
                .get(&x11sync::IDLETIME_COUNTER)
                .copied(),
            Some(0),
            "post-wake last_evaluated should be 0"
        );
    }

    #[test]
    fn key_event_fires_neg_transition_alarm_on_per_device_idletime_vck() {
        // Regression for the per-device fallback bug: a NegativeTransition
        // alarm on IDLETIME_DEVICE_VCK must fire on the very first input
        // even if `per_device_last_activity[3]` has no entry yet.
        // Without the fallback-to-global fix in the prologue, the computed
        // prior_device would be 0 and the trigger `old > wait && new <= wait`
        // would not hold — no AlarmNotify would reach the wire.
        //
        // PRIMARY assertion is AlarmNotify (type=84) on the client's
        // outbound stream; cache + state are secondary checks.
        use std::time::Duration;
        use yserver_protocol::x11::sync as x11sync;
        let mut state = ServerState::new();
        let mut peer = install_client(&mut state, 1);
        state.dpms.last_activity = std::time::Instant::now() - Duration::from_secs(90);
        assert!(
            state.per_device_last_activity.get(&3).is_none(),
            "test precondition: no per-device entry"
        );

        let alarm_id = 0x3000;
        state.sync_alarms.insert(
            alarm_id,
            crate::server::SyncAlarm {
                owner: ClientId(1),
                counter: x11sync::IDLETIME_DEVICE_VCK,
                wait_value: 60_000,
                delta: 0,
                test_type: x11sync::TEST_NEGATIVE_TRANSITION as u8,
                events: true, // load-bearing
                state: x11sync::ALARM_STATE_ACTIVE,
            },
        );
        let mut backend = crate::backend::recording::RecordingBackend::default();

        let _ = key_event_fanout_to_state(&mut state, &mut backend, key_event(true, 33));

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
                .get(&x11sync::IDLETIME_DEVICE_VCK)
                .copied(),
            Some(0)
        );
    }
}
