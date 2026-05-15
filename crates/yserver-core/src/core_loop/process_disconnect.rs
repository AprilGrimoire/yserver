//! Per-client disconnect cleanup, lifted out of `nested::handle_client`'s
//! closing block. Tears down every piece of state that referenced the
//! departing client (resources owned by it, per-client event masks,
//! grabs, selections, MIT-SHM segments, …) plus their host counterparts
//! (subwindows, fonts, pixmaps, RENDER pictures + glyphsets).
//!
//! Invoked from `run_core` on `Message::ClientDisconnected`, and also
//! when `process_request` reports `RequestOutcome::Disconnect` for a
//! peer that overflowed its outbound buffer.

use yserver_protocol::x11::{ClientId, ResourceId};

use crate::{
    backend::Backend,
    core_loop::fanout::{fanout_event_to_clients, subscribers_by_id},
    resources::{MapState, ROOT_WINDOW},
    server::ServerState,
};

/// One window's identity captured before the resource table forgets
/// it, so the post-mutation UnmapNotify+DestroyNotify fanout has
/// stable subscriber lists.
struct PendingDestroy {
    window: ResourceId,
    parent: ResourceId,
    was_mapped: bool,
    host_xid: Option<crate::backend::WindowHandle>,
    on_window: Vec<ClientId>,
    on_parent: Vec<ClientId>,
}

fn collect_destroy_order(
    table: &crate::resources::ResourceTable,
    root: ResourceId,
    out: &mut Vec<ResourceId>,
) {
    let Some(w) = table.window(root) else {
        return;
    };
    for child in w.children.clone() {
        collect_destroy_order(table, child, out);
    }
    out.push(root);
}

fn fanout_destroy_sequence(state: &mut ServerState, pending: &PendingDestroy) {
    let window = pending.window;
    let parent = pending.parent;
    if pending.was_mapped {
        let _dropped = fanout_event_to_clients(state, &pending.on_window, |buf, seq, order| {
            yserver_protocol::x11::encode_unmap_notify_event(
                buf, seq, order, window, window, false,
            );
        });
        let _dropped = fanout_event_to_clients(state, &pending.on_parent, |buf, seq, order| {
            yserver_protocol::x11::encode_unmap_notify_event(
                buf, seq, order, parent, window, false,
            );
        });
    }
    let _dropped = fanout_event_to_clients(state, &pending.on_window, |buf, seq, order| {
        yserver_protocol::x11::encode_destroy_notify_event(buf, seq, order, window, window);
    });
    let _dropped = fanout_event_to_clients(state, &pending.on_parent, |buf, seq, order| {
        yserver_protocol::x11::encode_destroy_notify_event(buf, seq, order, parent, window);
    });
}

/// Drop every server-side resource owned by `client_id` and free the
/// corresponding host objects.
///
/// If the client previously set `SetCloseDownMode(RetainPermanent |
/// RetainTemporary)`, its non-window resources (pixmaps, GCs, fonts,
/// cursors, pictures, glyphsets) survive with their original `owner:
/// ClientId` intact and the client_id is recorded in
/// `state.zombie_clients`. Connection-tied state (event masks, grabs,
/// selections, extension tables) is always torn down — a retained
/// client has no socket to receive on. The retained resources stay
/// findable by ID until either `KillClient(resource_owned_by_this_id)`
/// or, for RetainTemporary only, `KillClient(AllTemporary)`.
pub fn process_disconnect(state: &mut ServerState, backend: &mut dyn Backend, client_id: ClientId) {
    // Idempotent: a client can be disconnected twice in quick succession
    // (write-side EPIPE from process_request races the reader thread's
    // EOF → Message::ClientDisconnected). The first call removes the
    // entry from state.clients; the second sees None and bails.
    if !state.clients.contains_key(&client_id.0) {
        return;
    }
    let close_mode = state.close_down_modes.remove(&client_id.0).unwrap_or(0);
    let retain = close_mode == 1 || close_mode == 2;
    log::debug!(
        "process_disconnect: client {} close_mode={}",
        client_id.0,
        close_mode
    );
    // Send the reader thread (if any) a Shutdown so it exits cleanly
    // before we drop the client entry.
    if let Some(client) = state.clients.get(&client_id.0)
        && let Some(ctrl) = &client.reader_control
    {
        let _ = ctrl.send(crate::server::ReaderControl::Shutdown);
    }

    let mut owned_roots: Vec<ResourceId> = Vec::new();
    state
        .resources
        .collect_owned_window_roots(client_id, &mut owned_roots);

    let mut pending: Vec<PendingDestroy> = Vec::new();
    let mut all_destroyed: Vec<ResourceId> = Vec::new();
    for root in owned_roots {
        let mut order: Vec<ResourceId> = Vec::new();
        collect_destroy_order(&state.resources, root, &mut order);
        for w in &order {
            let (parent, was_mapped, host_xid) =
                state
                    .resources
                    .window(*w)
                    .map_or((ROOT_WINDOW, false, None), |win| {
                        (
                            win.parent,
                            win.map_state != MapState::Unmapped,
                            win.host_xid,
                        )
                    });
            let on_window = subscribers_by_id(state, *w, 0x0002_0000);
            let on_parent = subscribers_by_id(state, parent, 0x0008_0000);
            pending.push(PendingDestroy {
                window: *w,
                parent,
                was_mapped,
                host_xid,
                on_window,
                on_parent,
            });
        }
        let _ = state.resources.destroy_window(root);
        all_destroyed.extend(order);
    }
    state.drop_window_subscriptions(&all_destroyed);

    let removed = if retain {
        // Resources keep their original `owner: ClientId`. Tracking
        // the client_id in zombie_clients lets KillClient resolve
        // ownership back to this specific creator.
        state.zombie_clients.insert(client_id.0, close_mode);
        crate::resources::ClientRemovedResources::default()
    } else {
        state
            .resources
            .remove_non_window_resources_owned_by(client_id)
    };
    state.clients.remove(&client_id.0);

    let dead_windows: std::collections::HashSet<ResourceId> =
        all_destroyed.iter().copied().collect();
    state
        .xfixes_regions
        .retain(|_, region| region.owner != client_id);
    state
        .xfixes_selection_masks
        .retain(|(owner, _, _), _| *owner != client_id.0);
    state
        .xfixes_cursor_masks
        .retain(|(owner, _), _| *owner != client_id.0);
    state
        .shape_windows
        .retain(|window, _| !dead_windows.contains(window));
    state
        .shape_select_masks
        .retain(|(owner, window), _| *owner != client_id.0 && !dead_windows.contains(window));
    state
        .sync_counters
        .retain(|_, counter| counter.owner != client_id);
    state
        .sync_alarms
        .retain(|_, alarm| alarm.owner != client_id);
    state
        .sync_fences
        .retain(|_, fence| fence.owner != client_id);
    state.sync_pending_awaits.retain(|a| a.client != client_id);
    state.glx_contexts.retain(|_, c| c.owner != client_id);
    state.glx_drawables.retain(|_, d| d.owner != client_id);
    state
        .damage_objects
        .retain(|_, damage| damage.owner != client_id && !dead_windows.contains(&damage.drawable));
    // L2 plan B.1b: walk redirects owned by the departing client and
    // tear each one down. For single-window records the helper acts on
    // the named window directly; for `RedirectSubwindows` records the
    // backings live on the parent's *children* (per-child activation
    // in process_request), so the walk must descend into the children
    // instead of acting on the parent xid (which has no backing of its
    // own). Then filter by both ownership and dead-window so any
    // leftovers caught by the previous rule are still removed.
    let owned_redirects: Vec<(ResourceId, bool)> = state
        .composite_redirects
        .iter()
        .filter(|(_, rec)| rec.owner == client_id)
        .map(|((win, sub), _)| (*win, *sub))
        .collect();
    for (window, subwindows) in &owned_redirects {
        if *subwindows {
            let children: Vec<ResourceId> = state.resources.children(*window).to_vec();
            for child in children {
                teardown_redirect_for_window(state, backend, child);
            }
        } else {
            teardown_redirect_for_window(state, backend, *window);
        }
    }
    state
        .composite_redirects
        .retain(|(window, _), rec| rec.owner != client_id && !dead_windows.contains(window));
    state.present_event_selections.retain(|_, selection| {
        selection.owner != client_id && !dead_windows.contains(&selection.window)
    });
    state
        .present_msc
        .retain(|window, _| !dead_windows.contains(window));
    state
        .mit_shm_segments
        .retain(|_, seg| seg.owner != client_id);
    state
        .randr_select_masks
        .retain(|(owner, window), _| *owner != client_id.0 && !dead_windows.contains(window));
    state
        .xkb_select_event_masks
        .retain(|(owner, _), _| *owner != client_id.0);
    state.button_grabs.retain(|g| g.owner != client_id);
    if state
        .pointer_grab
        .is_some_and(|(owner, _)| owner == client_id)
    {
        state.pointer_grab = None;
        state.pointer_grab_is_passive = false;
        state.frozen_pointer_event = None;
    }
    state
        .selections
        .retain(|_, owner_window| !dead_windows.contains(owner_window));

    // Host-side teardown. Order matches `nested::handle_client`'s tail
    // so behavior is bit-identical.
    for entry in pending {
        if let Some(xid) = entry.host_xid {
            let _ = backend.destroy_subwindow(None, xid.as_raw());
            backend.unregister_host_window(xid.as_raw());
        }
        fanout_destroy_sequence(state, &entry);
    }
    for xid in removed.closed_fonts {
        let _ = backend.close_font(None, xid);
    }
    for xid in removed.freed_pixmaps {
        let _ = backend.free_pixmap(None, xid);
    }
    for (pic_xid, owned_pix) in removed.freed_pictures {
        let _ = backend.render_free_picture(None, pic_xid);
        if let Some(pix_xid) = owned_pix {
            let _ = backend.free_pixmap(None, pix_xid);
        }
    }
    for gs_xid in removed.freed_glyphsets {
        let _ = backend.render_free_glyphset(None, gs_xid);
    }
    for cursor_xid in removed.freed_cursors {
        let _ = backend.free_cursor(None, cursor_xid);
    }
}

/// L2 plan B.6c — release the redirect's reason-1 hold on a
/// window's off-screen backing. Takes `Window.redirected_backing`
/// off the resource record; asks the backend to decref the
/// alias-registry entry (and free the underlying pixmap when the
/// last ref drops). Surviving `NameWindowPixmap` aliases keep the
/// backing alive until their `FreePixmap` lands.
///
/// Shared by both the COMPOSITE `UnredirectWindow` /
/// `UnredirectSubwindows` dispatch arm in
/// `crates/yserver-core/src/core_loop/process_request.rs` and the
/// per-client disconnect cleanup above.
pub(crate) fn teardown_redirect_for_window(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    window: ResourceId,
) {
    // Snapshot host_xid + backing in one borrow scope BEFORE `.take()`.
    // The scanout-skip toggle needs the window's host XID and the
    // backing-release needs the pixmap handle; reading both up-front
    // keeps the lifecycle obvious (host_xid is None ⇒ backing must
    // also be None, since activate requires host_xid).
    let snapshot = state.resources.window_mut(window).map(|w| {
        let host_xid = w.host_xid;
        let backing = w.redirected_backing.take();
        (host_xid, backing)
    });
    let Some((host_xid, Some(backing))) = snapshot else {
        return;
    };
    if let Err(err) = backend.release_redirected_backing(None, backing.host_pixmap) {
        log::warn!(
            "release_redirected_backing(0x{:x}) failed: {err}",
            backing.host_pixmap.as_raw()
        );
    }
    if let Some(host_window) = host_xid {
        backend.set_window_scanout_skipped(None, host_window, false);
    }
}

/// Destroy every resource owned by a zombie client — invoked by
/// `KillClient(AllTemporary)` (for each RetainTemporary zombie) and by
/// `KillClient(resource_owned_by_a_zombie)`. Mirrors the resource-
/// destroy half of `process_disconnect`, minus the live-client setup
/// (there is no `state.clients` entry, no reader thread, no
/// connection-tied extension state — those were torn down at the
/// original disconnect). The caller must remove `zombie` from
/// `state.zombie_clients` after this returns.
pub fn destroy_zombie_resources(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    zombie: ClientId,
) {
    let mut owned_roots: Vec<ResourceId> = Vec::new();
    state
        .resources
        .collect_owned_window_roots(zombie, &mut owned_roots);

    let mut pending: Vec<PendingDestroy> = Vec::new();
    let mut all_destroyed: Vec<ResourceId> = Vec::new();
    for root in owned_roots {
        let mut order: Vec<ResourceId> = Vec::new();
        collect_destroy_order(&state.resources, root, &mut order);
        for w in &order {
            let (parent, was_mapped, host_xid) =
                state
                    .resources
                    .window(*w)
                    .map_or((ROOT_WINDOW, false, None), |win| {
                        (
                            win.parent,
                            win.map_state != MapState::Unmapped,
                            win.host_xid,
                        )
                    });
            let on_window = subscribers_by_id(state, *w, 0x0002_0000);
            let on_parent = subscribers_by_id(state, parent, 0x0008_0000);
            pending.push(PendingDestroy {
                window: *w,
                parent,
                was_mapped,
                host_xid,
                on_window,
                on_parent,
            });
        }
        let _ = state.resources.destroy_window(root);
        all_destroyed.extend(order);
    }
    state.drop_window_subscriptions(&all_destroyed);

    let removed = state.resources.remove_non_window_resources_owned_by(zombie);

    for entry in pending {
        if let Some(xid) = entry.host_xid {
            let _ = backend.destroy_subwindow(None, xid.as_raw());
            backend.unregister_host_window(xid.as_raw());
        }
        fanout_destroy_sequence(state, &entry);
    }
    for xid in removed.closed_fonts {
        let _ = backend.close_font(None, xid);
    }
    for xid in removed.freed_pixmaps {
        let _ = backend.free_pixmap(None, xid);
    }
    for (pic_xid, owned_pix) in removed.freed_pictures {
        let _ = backend.render_free_picture(None, pic_xid);
        if let Some(pix_xid) = owned_pix {
            let _ = backend.free_pixmap(None, pix_xid);
        }
    }
    for gs_xid in removed.freed_glyphsets {
        let _ = backend.render_free_glyphset(None, gs_xid);
    }
    for cursor_xid in removed.freed_cursors {
        let _ = backend.free_cursor(None, cursor_xid);
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{HashMap, HashSet, VecDeque},
        os::unix::net::UnixStream,
        sync::{Arc, Mutex, atomic::AtomicU16},
    };

    use yserver_protocol::x11::{ClientByteOrder, ClientId, CreatePixmapRequest, ResourceId};

    use super::{destroy_zombie_resources, process_disconnect};
    use crate::{
        backend::recording::RecordingBackend,
        resources::ROOT_WINDOW,
        server::{ClientState, CompositeRedirectMode, RedirectRecord, ServerState},
    };

    fn install_client(state: &mut ServerState, id: u32) {
        let (a, _b) = UnixStream::pair().expect("socketpair");
        state.clients.insert(
            id,
            ClientState {
                writer: Arc::new(Mutex::new(a)),
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: Arc::new(AtomicU16::new(0)),
                resource_id_base: id << 20,
                resource_id_mask: 0x000F_FFFF,
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
    }

    #[test]
    fn disconnect_leaves_other_clients_redirects_intact() {
        let mut state = ServerState::new();
        install_client(&mut state, 1);
        install_client(&mut state, 2);
        // Client A redirects window W.
        state.composite_redirects.insert(
            (ResourceId(0x1234), false),
            RedirectRecord {
                mode: CompositeRedirectMode::Manual,
                owner: ClientId(1),
            },
        );
        let mut backend = RecordingBackend::new();
        process_disconnect(&mut state, &mut backend, ClientId(2));
        assert!(
            state
                .composite_redirects
                .contains_key(&(ResourceId(0x1234), false))
        );
    }

    #[test]
    fn disconnect_tears_down_owned_redirect() {
        let mut state = ServerState::new();
        install_client(&mut state, 1);
        state.composite_redirects.insert(
            (ResourceId(0x5678), false),
            RedirectRecord {
                mode: CompositeRedirectMode::Manual,
                owner: ClientId(1),
            },
        );
        let mut backend = RecordingBackend::new();
        process_disconnect(&mut state, &mut backend, ClientId(1));
        assert!(state.composite_redirects.is_empty());
    }

    #[test]
    fn disconnect_with_subwindows_redirect_walks_children() {
        // A `RedirectSubwindows(P, Manual)` record's key window is P,
        // not the children. The per-child backings are attached to P's
        // direct children, so a disconnect walk that only acts on
        // `record.window` would skip them and leak backings. The fix
        // descends into `state.resources.children(P)`.
        use crate::{
            backend::{WindowHandle, recording::RecordedCall},
            resources::RedirectedBacking,
        };
        use yserver_protocol::x11::CreateWindowRequest;

        let mut state = ServerState::new();
        // Client 1 is the compositor that requested RedirectSubwindows.
        // Client 2 owns the actual top-level windows — they survive
        // client 1's disconnect (which is the realistic flow:
        // xfwm4 dies, app windows stay).
        install_client(&mut state, 1);
        install_client(&mut state, 2);
        // Two direct children of root owned by client 2, each with a
        // host xid + an already-allocated backing (simulates the
        // post-activation state right before client 1 disconnects).
        for (id, host_xid, backing_xid) in [
            (0x0200_0080u32, 0x0220_0080u32, 0x0320_0080u32),
            (0x0200_0081, 0x0220_0081, 0x0320_0081),
        ] {
            state.resources.create_window(
                ClientId(2),
                CreateWindowRequest {
                    window: ResourceId(id),
                    parent: ROOT_WINDOW,
                    x: 0,
                    y: 0,
                    width: 80,
                    height: 80,
                    border_width: 0,
                    depth: 32,
                    visual: ResourceId(0),
                    class: 0,
                    background_pixel: None,
                    bit_gravity: None,
                    win_gravity: None,
                    backing_store: None,
                    backing_planes: None,
                    backing_pixel: None,
                    override_redirect: Some(true),
                    save_under: None,
                    event_mask: None,
                    do_not_propagate_mask: None,
                    colormap: None,
                },
            );
            if let Some(w) = state.resources.window_mut(ResourceId(id)) {
                w.host_xid = Some(WindowHandle::from_raw_panicking(host_xid));
                w.redirected_backing = Some(RedirectedBacking {
                    host_pixmap: crate::backend::PixmapHandle::from_raw_panicking(backing_xid),
                    width: 80,
                    height: 80,
                    depth: 32,
                });
            }
        }
        state.composite_redirects.insert(
            (ROOT_WINDOW, true),
            RedirectRecord {
                mode: CompositeRedirectMode::Manual,
                owner: ClientId(1),
            },
        );
        let mut backend = RecordingBackend::new();
        process_disconnect(&mut state, &mut backend, ClientId(1));

        // Record removed; both children's backings released and
        // their scanout-skip flags cleared. Children themselves
        // survive (owned by client 2, not client 1).
        assert!(state.composite_redirects.is_empty());
        for id in [0x0200_0080u32, 0x0200_0081] {
            assert!(
                state.resources.window(ResourceId(id)).is_some(),
                "child 0x{id:x} should outlive client 1's disconnect",
            );
            assert!(
                state
                    .resources
                    .window(ResourceId(id))
                    .and_then(|w| w.redirected_backing.as_ref())
                    .is_none(),
                "redirected_backing must be cleared for child 0x{id:x}",
            );
        }
        let calls = backend.calls.lock().unwrap().clone();
        let releases: Vec<u32> = calls
            .iter()
            .filter_map(|c| match c {
                RecordedCall::ReleaseRedirectedBacking(raw) => Some(*raw),
                _ => None,
            })
            .collect();
        assert!(
            releases.contains(&0x0320_0080),
            "child 1 backing not released"
        );
        assert!(
            releases.contains(&0x0320_0081),
            "child 2 backing not released"
        );
        let clears: Vec<u32> = calls
            .iter()
            .filter_map(|c| match c {
                RecordedCall::SetWindowScanoutSkipped {
                    host_window,
                    skip: false,
                } => Some(*host_window),
                _ => None,
            })
            .collect();
        assert!(
            clears.contains(&0x0220_0080),
            "child 1 scanout-skip not cleared"
        );
        assert!(
            clears.contains(&0x0220_0081),
            "child 2 scanout-skip not cleared"
        );
    }

    #[test]
    fn disconnect_with_retain_permanent_keeps_pixmap_owned_by_original_client() {
        let mut state = ServerState::new();
        let mut backend = RecordingBackend::new();
        install_client(&mut state, 7);
        state.resources.create_pixmap(
            ClientId(7),
            CreatePixmapRequest {
                pixmap: ResourceId(0x0070_0001),
                drawable: ROOT_WINDOW,
                width: 16,
                height: 16,
                depth: 24,
            },
        );
        state.close_down_modes.insert(7, 1);

        process_disconnect(&mut state, &mut backend, ClientId(7));

        // Pixmap survives, owner field unchanged.
        assert_eq!(
            state.resources.resource_owner(ResourceId(0x0070_0001)),
            Some(ClientId(7)),
        );
        // Client is gone from the live map, recorded as zombie with
        // the original close-down mode (RetainPermanent = 1).
        assert!(!state.clients.contains_key(&7));
        assert!(!state.close_down_modes.contains_key(&7));
        assert_eq!(state.zombie_clients.get(&7).copied(), Some(1));
    }

    #[test]
    fn disconnect_with_destroy_default_frees_pixmap() {
        let mut state = ServerState::new();
        let mut backend = RecordingBackend::new();
        install_client(&mut state, 7);
        state.resources.create_pixmap(
            ClientId(7),
            CreatePixmapRequest {
                pixmap: ResourceId(0x0070_0001),
                drawable: ROOT_WINDOW,
                width: 16,
                height: 16,
                depth: 24,
            },
        );

        process_disconnect(&mut state, &mut backend, ClientId(7));

        assert!(
            state
                .resources
                .resource_owner(ResourceId(0x0070_0001))
                .is_none()
        );
        assert!(!state.zombie_clients.contains_key(&7));
    }

    #[test]
    fn destroy_zombie_resources_frees_only_targeted_clients_resources() {
        // Regression: previously two retained clients (32 and 33) shared
        // a single retain bucket, so killing one leaked the other's
        // resources. Now they keep their original owner — killing one
        // must not touch the other.
        let mut state = ServerState::new();
        let mut backend = RecordingBackend::new();
        state.resources.create_pixmap(
            ClientId(32),
            CreatePixmapRequest {
                pixmap: ResourceId(0x0200_0001),
                drawable: ROOT_WINDOW,
                width: 16,
                height: 16,
                depth: 24,
            },
        );
        state.resources.create_pixmap(
            ClientId(33),
            CreatePixmapRequest {
                pixmap: ResourceId(0x0210_0001),
                drawable: ROOT_WINDOW,
                width: 16,
                height: 16,
                depth: 24,
            },
        );
        state.zombie_clients.insert(32, 1);
        state.zombie_clients.insert(33, 1);

        destroy_zombie_resources(&mut state, &mut backend, ClientId(32));

        assert!(
            state
                .resources
                .resource_owner(ResourceId(0x0200_0001))
                .is_none()
        );
        assert_eq!(
            state.resources.resource_owner(ResourceId(0x0210_0001)),
            Some(ClientId(33)),
        );
    }
}
