//! `Backend` trait — the surface `process_request` and the core loop
//! call on the host backend. Exists primarily as a seam for testing
//! (`RecordingBackend` lives next door, gated `#[cfg(test)]`) and so
//! that the KMS backend can sit alongside the host-X11 backend without
//! touching every call site.
//!
//! Method signatures mirror the existing `HostX11Backend::*` methods
//! 1:1 — no `Param` structs, no parameter renaming. Several methods
//! still take raw `u32` host xids rather than handle newtypes — call
//! sites pass `u32` from the `ResourceTable`'s `host_xid` field, and
//! rewrapping/unwrapping at every call boundary is noise.

use std::{any::Any, io};

use yserver_protocol::x11::{ClipRectangles, FontMetrics, xfixes};

use crate::{
    backend::{
        AnyHandle, ClipState, CursorHandle, DrawState, FillState, FontHandle, GlyphSetHandle,
        OriginContext, PictureHandle, PixmapHandle, WindowHandle,
    },
    core_loop::HostInputEvent,
    host_x11::{HostEvent, HostSubwindowConfig, HostSubwindowVisual, HostXidMap, PointerPosition},
    server::ServerState,
};

use yserver_protocol::x11::ResourceId;

/// Categorises the raw fds a backend wants the core's mio poller to
/// watch on its behalf (returned by `Backend::poll_fds`).
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum BackendFdKind {
    /// libinput's epoll fd. Readiness is dispatched to the libinput
    /// thread (KMS) — but the fd inventory still flows through the
    /// trait so the core's poller can register it uniformly.
    Libinput,
    /// DRM device fd; readiness drives `on_page_flip_ready`.
    Drm,
    /// Host X11 connection fd (ynest only); readiness drives
    /// `Backend::drain_host_socket` on the core thread.
    HostX11,
}

/// Outcome of a single `Backend::drain_host_socket` pass. Re-exported
/// from `host_x11` so non-host-X11 backends can spell the type
/// without depending on the host module.
pub use crate::host_x11::HostSocketStatus;

/// Present capability surface. Phase 4.2 design §4. Per-window
/// because Present's `QueryCapabilities` is per-window in the wire
/// protocol; in single-output single-GPU configurations the same
/// values are returned for every window.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PresentCaps {
    /// Both KMS plane `IN_FENCE_FD` property *and* CRTC `OUT_FENCE_PTR`
    /// are available; explicit-fence flip handshake works. When
    /// false, the path selector short-circuits to `Copy` regardless
    /// of pixmap/window match per design §3.3.1.
    pub flip_path: bool,
    /// `DRM_MODE_ATOMIC_NONBLOCK` accepted by the kernel — the
    /// `PresentOptionAsyncMayTear` bit is honourable. When false,
    /// the bit is **silently cleared** on incoming requests (per
    /// design §4 row "Kernel rejects DRM_MODE_ATOMIC_NONBLOCK"),
    /// not folded into `PresentOptionAsync`.
    pub async_may_tear: bool,
    /// `Dri3Caps::syncobj` mirror — Present syncobj cap requires
    /// DRI3 syncobj support to be useful, so we don't advertise it
    /// without DRI3-side timeline plumbing.
    pub syncobj: bool,
}

impl PresentCaps {
    /// Encode as a `u32` for the `QueryCapabilities` reply.
    /// `presentproto` bit assignments:
    ///  - `Async` (0x1)        — Phase 4.2 advertises if `flip_path`.
    ///  - `Fence` (0x2)        — XSync `Fence` always supported once
    ///    DRI3 fence_fd cap is true; Phase 4.2 always advertises.
    ///  - `UST` (0x4)          — vblank UST timestamps; Phase 4.1's
    ///    pageflip path already produces these.
    ///  - `AsyncMayTear` (0x8) — `async_may_tear`.
    ///  - `Syncobj` (0x10)     — `syncobj`.
    #[must_use]
    pub fn encode(self) -> u32 {
        const ASYNC: u32 = 0x1;
        const FENCE: u32 = 0x2;
        const UST: u32 = 0x4;
        const ASYNC_MAY_TEAR: u32 = 0x8;
        const SYNCOBJ: u32 = 0x10;
        let mut out = FENCE | UST;
        if self.flip_path {
            out |= ASYNC;
        }
        if self.async_may_tear {
            out |= ASYNC_MAY_TEAR;
        }
        if self.syncobj {
            out |= SYNCOBJ;
        }
        out
    }
}

#[cfg(test)]
mod present_caps_tests {
    use super::PresentCaps;

    #[test]
    fn default_advertises_fence_and_ust() {
        // Default = all-false: still advertises Fence + UST per
        // Phase 4.2 (Fence is from XSync, UST from existing pageflip
        // path), but not Async / AsyncMayTear / Syncobj.
        let bits = PresentCaps::default().encode();
        assert_eq!(bits & 0x1, 0, "Async unset without flip_path");
        assert_eq!(bits & 0x2, 0x2, "Fence advertised");
        assert_eq!(bits & 0x4, 0x4, "UST advertised");
        assert_eq!(bits & 0x8, 0, "AsyncMayTear unset");
        assert_eq!(bits & 0x10, 0, "Syncobj unset");
    }

    #[test]
    fn full_caps_set_all_bits() {
        let caps = PresentCaps {
            flip_path: true,
            async_may_tear: true,
            syncobj: true,
        };
        assert_eq!(caps.encode(), 0x1 | 0x2 | 0x4 | 0x8 | 0x10);
    }
}

/// DRI3 capability surface. Phase 4.2 design §4. `version == (0, 0)`
/// is the "DRI3 unsupported" sentinel; any other value advertises
/// DRI3 to clients. The other booleans gate individual request types
/// rather than the whole extension.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Dri3Caps {
    /// Max DRI3 version this backend can serve. `(0, 0)` = unsupported.
    pub version: (u32, u32),
    /// `VK_EXT_image_drm_format_modifier` enabled — non-LINEAR
    /// modifiers can be imported. When false, `GetSupportedModifiers`
    /// only advertises `DRM_FORMAT_MOD_LINEAR`.
    pub modifiers: bool,
    /// `VK_KHR_external_semaphore_fd` with `SYNC_FD` handle type
    /// available — XSync `Fence` import via `FenceFromFD` works.
    /// When false, those requests reject with `BadImplementation`
    /// per the §4 fallback matrix.
    pub fence_fd: bool,
    /// `VK_KHR_external_semaphore_fd` `OPAQUE_FD` +
    /// `VK_KHR_timeline_semaphore` + DRM_SYNCOBJ ioctls all
    /// available. When false, `ImportSyncobj` / `FreeSyncobj` reject
    /// and the advertised version caps at `(1, 3)`.
    pub syncobj: bool,
}

impl Dri3Caps {
    /// The "DRI3 unsupported" sentinel. Used as the default-impl
    /// return value on backends that don't speak DRI3.
    #[must_use]
    pub const fn unsupported() -> Self {
        Self {
            version: (0, 0),
            modifiers: false,
            fence_fd: false,
            syncobj: false,
        }
    }
}

/// The dynamic backend surface. `Send` is required so that
/// `Arc<Mutex<dyn Backend>>` is `Send + Sync` (`Mutex<T>` is Sync iff
/// `T: Send`). `Sync` on the trait itself is not required because all
/// `Backend` access is mediated through a `Mutex`.
pub trait Backend: Send {
    // ──────────────────────────────────────────────────────────────
    // Lifecycle / state accessors
    // ──────────────────────────────────────────────────────────────

    fn window_id(&self) -> u32;
    fn root_visual_xid(&self) -> u32;
    fn argb_visual_xid(&self) -> Option<u32>;
    fn argb_colormap_xid(&self) -> Option<u32>;
    fn render_opcode(&self) -> Option<u8>;
    fn xkb_opcode(&self) -> Option<u8>;
    fn xkb_info(&self) -> Option<(u8, u8, u8)>;
    fn composite_opcode(&self) -> Option<u8>;
    fn render_format_for_ynest_id(&self, ynest_fmt: u32) -> Option<u32>;
    fn ping(&mut self, origin: Option<OriginContext>) -> io::Result<()>;

    // ──────────────────────────────────────────────────────────────
    // Single-threaded core hooks
    // ──────────────────────────────────────────────────────────────

    /// Dispatch a host input event the core received over the
    /// `Message::HostInput` channel. The backend produces zero or more
    /// X11 wire events via `state` fanout helpers. Filled in by E2
    /// (KMS) and F2 (host-X11); inert until then.
    fn on_host_input(&mut self, state: &mut ServerState, ev: HostInputEvent);

    /// DRM page-flip completion fd is readable. The backend should
    /// drain completion events and submit the next composite/flip.
    fn on_page_flip_ready(&mut self, state: &mut ServerState);

    /// Tell the backend that something that could affect on-screen
    /// pixels has changed since the last composite. KMS uses this to
    /// gate `composite_and_flip` so an idle server doesn't burn
    /// vsync-paced composite cycles. Default is a no-op for backends
    /// (e.g. host-X11) that don't drive their own composite loop.
    fn mark_dirty(&mut self) {}

    /// If the backend has pending damage and no flip is currently in
    /// flight, kick the next composite. Called once per core-loop
    /// iteration so a backend that went dormant after the last
    /// pageflip-complete (because nothing was dirty) wakes back up
    /// when fresh damage arrives. Default no-op.
    ///
    /// # Errors
    ///
    /// Propagates whatever the backend's composite path returns
    /// (e.g. DRM/Vulkan submit errors on KMS).
    fn maybe_composite(&mut self) -> io::Result<()> {
        Ok(())
    }

    /// Dump the current scanout buffer to a file in the process's
    /// cwd. Diagnostic-only — fired from `Message::DumpScanout`
    /// (SIGUSR1 on KMS). Default no-op for backends that don't
    /// drive their own composite.
    fn dump_scanout(&mut self) {}

    /// Raw fds the core's poller should watch on this backend's behalf.
    /// The core registers each fd against the matching token derived
    /// from `BackendFdKind`.
    fn poll_fds(&self) -> Vec<(std::os::fd::RawFd, BackendFdKind)>;

    /// Downcast to `Any` for backend-specific operations (e.g. KMS composite).
    fn as_any(&self) -> &dyn Any;
    /// Mutable downcast to `Any` for backend-specific operations.
    fn as_any_mut(&mut self) -> &mut dyn Any;

    // ──────────────────────────────────────────────────────────────
    // Subwindow lifecycle
    // ──────────────────────────────────────────────────────────────

    #[allow(clippy::too_many_arguments)]
    fn create_subwindow(
        &mut self,
        origin: Option<OriginContext>,
        host_parent: WindowHandle,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        border_width: u16,
        visual: HostSubwindowVisual,
        background_pixel: Option<u32>,
        background_pixmap: Option<u32>,
    ) -> io::Result<WindowHandle>;

    fn destroy_subwindow(&mut self, origin: Option<OriginContext>, host_xid: u32)
    -> io::Result<()>;

    fn map_subwindow(&mut self, origin: Option<OriginContext>, host_xid: u32) -> io::Result<()>;

    fn unmap_subwindow(&mut self, origin: Option<OriginContext>, host_xid: u32) -> io::Result<()>;

    fn configure_subwindow(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        config: HostSubwindowConfig,
    ) -> io::Result<()>;

    fn reparent_subwindow(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        host_parent: u32,
        x: i16,
        y: i16,
    ) -> io::Result<()>;

    fn change_subwindow_attributes(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        value_mask: u32,
        values: &[u32],
    ) -> io::Result<()>;

    fn update_host_event_mask(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        mask: u32,
        enabled: bool,
    ) -> io::Result<()>;

    /// Phase 6.3 Step 4: register a freshly-created host top-level so
    /// pointer / expose events on it route to `nested_id` through
    /// `pointer_event_fanout` / `expose_event_fanout`. Replaces the
    /// pre-Step-4 pump-handle `register_top_level`.
    fn register_top_level(
        &mut self,
        origin: Option<OriginContext>,
        nested_id: ResourceId,
        host_xid: u32,
    ) -> io::Result<()>;

    /// Phase 6.3 Step 4: same as [`Backend::register_top_level`] but
    /// for sub-windows — selects `Exposure` only so pointer events
    /// bubble up to the top-level ancestor where dispatch lives.
    fn register_subwindow(
        &mut self,
        origin: Option<OriginContext>,
        nested_id: ResourceId,
        host_xid: u32,
    ) -> io::Result<()>;

    /// Phase 6.3 Step 4: drop a `host_xid → ResourceId` mapping at
    /// DestroyWindow / Reparent-out so stale host events never
    /// misroute.
    fn unregister_host_window(&mut self, host_xid: u32);

    /// View of the `host_xid → ResourceId` map. F2: plain HashMap —
    /// the core thread is the only writer/reader, so no Arc/Mutex.
    fn xid_map(&self) -> &HostXidMap;

    /// F2: drain whatever the kernel has buffered on the host fd and
    /// classify into `pending_replies` / `pending_events`. KMS
    /// backend has no host fd; the default no-op suits.
    fn drain_host_socket(&mut self) -> io::Result<HostSocketStatus> {
        Ok(HostSocketStatus::WouldBlock)
    }

    /// F2: pop the next decoded host event so the core can fan out at
    /// the outer-loop boundary. Default `None` — only HostX11Backend
    /// produces these.
    fn pop_pending_host_event(&mut self) -> Option<HostEvent> {
        None
    }

    /// F2: did the host close the connection? `run_core` posts
    /// `Message::Shutdown` once this flips true.
    fn host_socket_eof(&self) -> bool {
        false
    }

    fn name_window_pixmap(
        &mut self,
        origin: Option<OriginContext>,
        host_window: WindowHandle,
    ) -> io::Result<PixmapHandle>;

    /// L2 plan B.6a: allocate the off-screen mirror that backs a
    /// newly redirected window. The backend allocates a fresh pixmap
    /// handle, sets up a mirror at `(width, height)` for `depth`,
    /// and seeds the alias registry's refcount=1 hold (reason 1 —
    /// "window is redirected"). Subsequent
    /// [`Self::name_window_pixmap`] calls on the same `host_window`
    /// return this same handle with an incremented refcount.
    ///
    /// Default `Unsupported` so test doubles and host-X11 backends
    /// don't have to implement it until they grow real redirect
    /// support.
    ///
    /// # Errors
    ///
    /// Returns `io::ErrorKind::Unsupported` when the backend hasn't
    /// implemented redirect activation. KMS overrides this in
    /// `crates/yserver/src/kms/backend.rs`.
    fn allocate_redirected_backing(
        &mut self,
        origin: Option<OriginContext>,
        host_window: WindowHandle,
        width: u16,
        height: u16,
        depth: u8,
    ) -> io::Result<PixmapHandle> {
        let _ = (origin, host_window, width, height, depth);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "allocate_redirected_backing not supported",
        ))
    }

    /// L2 plan B.6c: release a redirect's reason-1 hold on a
    /// backing pixmap. The backend decrements the alias registry's
    /// refcount and frees the underlying pixmap if no
    /// `NameWindowPixmap` alias holds it. Surviving aliases keep
    /// it alive until their own `FreePixmap`.
    ///
    /// Default no-op so test doubles and host-X11 backends without
    /// redirect support don't have to track aliases.
    ///
    /// # Errors
    ///
    /// Propagates backend errors from the underlying free path
    /// (e.g. KMS's `free_pixmap` on the final reference drop).
    fn release_redirected_backing(
        &mut self,
        origin: Option<OriginContext>,
        backing: PixmapHandle,
    ) -> io::Result<()> {
        let _ = (origin, backing);
        Ok(())
    }

    /// Mark `host_window` as redirected-Manual (or clear the mark).
    /// While marked, the backend's scanout pass must skip the window's
    /// own mirror — the compositor that issued `RedirectWindow` /
    /// `RedirectSubwindows update=Manual(1)` is responsible for
    /// painting the window's visible content to the root via
    /// `RENDER Composite` from a `NameWindowPixmap` alias of the
    /// backing.
    ///
    /// Called from the COMPOSITE activation / teardown / mode-
    /// transition paths in `crates/yserver-core/src/core_loop/process_request.rs`
    /// and from `teardown_redirect_for_window` in
    /// `process_disconnect.rs`. Kept as an explicit toggle rather than
    /// piggybacking on `allocate_redirected_backing` /
    /// `release_redirected_backing` so resize (which releases the old
    /// backing while the window stays redirected) cannot accidentally
    /// clear the flag.
    ///
    /// Default no-op for backends without a scanout pass of their own
    /// (host-X11 forwards paint to the host server, so the host
    /// performs its own composite).
    fn set_window_scanout_skipped(
        &mut self,
        origin: Option<OriginContext>,
        host_window: WindowHandle,
        skip: bool,
    ) {
        let _ = (origin, host_window, skip);
    }

    // ──────────────────────────────────────────────────────────────
    // Resources (pixmap, font, cursor)
    // ──────────────────────────────────────────────────────────────

    fn create_pixmap(
        &mut self,
        origin: Option<OriginContext>,
        depth: u8,
        width: u16,
        height: u16,
    ) -> io::Result<PixmapHandle>;

    fn free_pixmap(&mut self, origin: Option<OriginContext>, host_xid: u32) -> io::Result<()>;

    fn open_font(
        &mut self,
        origin: Option<OriginContext>,
        name: &str,
    ) -> io::Result<(FontHandle, FontMetrics)>;

    fn close_font(&mut self, origin: Option<OriginContext>, host_xid: u32) -> io::Result<()>;

    fn create_cursor(
        &mut self,
        origin: Option<OriginContext>,
        source_pixmap: PixmapHandle,
        mask_pixmap: Option<PixmapHandle>,
        fore: (u16, u16, u16),
        back: (u16, u16, u16),
        hot_x: u16,
        hot_y: u16,
    ) -> io::Result<CursorHandle>;

    /// Free a host cursor. Counterpart to `create_cursor`. Default impl
    /// is a no-op so backends without a real X-server peer (KMS) need
    /// no boilerplate.
    fn free_cursor(&mut self, _origin: Option<OriginContext>, _host_xid: u32) -> io::Result<()> {
        Ok(())
    }

    /// Create a cursor from two glyph indices in two fonts (X11 core
    /// `CreateGlyphCursor`, opcode 94). The protocol does not carry an
    /// explicit hotspot — the backend computes one from the source
    /// glyph's metrics (origin point of the source glyph in the
    /// rendered cursor pixmap).
    ///
    /// `mask_font` is `None` when the wire request had `mask = None`,
    /// in which case the source glyph doubles as the mask: cursor
    /// pixel is visible iff the source bit is set, and visible pixels
    /// always carry `fore`.
    fn create_glyph_cursor(
        &mut self,
        origin: Option<OriginContext>,
        source_font: FontHandle,
        mask_font: Option<FontHandle>,
        source_char: u16,
        mask_char: u16,
        fore: (u16, u16, u16),
        back: (u16, u16, u16),
    ) -> io::Result<CursorHandle>;

    fn define_cursor(
        &mut self,
        origin: Option<OriginContext>,
        host_window_xid: u32,
        cursor_host_xid: u32,
    ) -> io::Result<()>;

    // ──────────────────────────────────────────────────────────────
    // Container background (root-mapped helpers)
    // ──────────────────────────────────────────────────────────────

    fn set_container_background_pixel(
        &mut self,
        origin: Option<OriginContext>,
        pixel: u32,
    ) -> io::Result<()>;

    fn set_container_background_pixmap(
        &mut self,
        origin: Option<OriginContext>,
        host_pixmap_xid: u32,
    ) -> io::Result<()>;

    // ──────────────────────────────────────────────────────────────
    // GC state (sync points feeding the host's shared GC)
    // ──────────────────────────────────────────────────────────────

    fn clear_clip_rectangles(&mut self, origin: Option<OriginContext>) -> io::Result<()>;

    fn set_clip_rectangles(
        &mut self,
        origin: Option<OriginContext>,
        clip: Option<ClipRectangles>,
    ) -> io::Result<()>;

    fn set_clip_pixmap(
        &mut self,
        origin: Option<OriginContext>,
        host_pixmap: u32,
        clip_x_origin: i16,
        clip_y_origin: i16,
    ) -> io::Result<()>;

    fn set_gc_fill_solid(&mut self, origin: Option<OriginContext>) -> io::Result<()>;

    fn set_gc_fill_tiled(
        &mut self,
        origin: Option<OriginContext>,
        host_pixmap: u32,
        tile_x_origin: i16,
        tile_y_origin: i16,
    ) -> io::Result<()>;

    fn apply_clip_state(
        &mut self,
        origin: Option<OriginContext>,
        clip: &ClipState,
    ) -> io::Result<()>;

    fn apply_fill_state(
        &mut self,
        origin: Option<OriginContext>,
        fill: &FillState,
    ) -> io::Result<()>;

    fn apply_draw_state(
        &mut self,
        origin: Option<OriginContext>,
        state: &DrawState,
    ) -> io::Result<()>;

    // ──────────────────────────────────────────────────────────────
    // Drawing primitives
    //
    // These match the existing `HostX11Backend::*` signatures: they
    // take raw `u32` host xids and a foreground colour (the Phase 6.2
    // additive scope adds `&DrawState` propagation through the
    // `apply_draw_state` sync point above; the methods themselves are
    // unchanged). Phase 6.3+ may collapse the surface as the trait
    // grows additional impls, but for now matching the existing shape
    // keeps Step 5 churn-free at every call site.
    // ──────────────────────────────────────────────────────────────

    #[allow(clippy::too_many_arguments)]
    fn copy_area(
        &mut self,
        origin: Option<OriginContext>,
        src_host_xid: u32,
        dst_host_xid: u32,
        src_x: i16,
        src_y: i16,
        dst_x: i16,
        dst_y: i16,
        width: u16,
        height: u16,
    ) -> io::Result<()>;

    #[allow(clippy::too_many_arguments)]
    fn copy_plane(
        &mut self,
        origin: Option<OriginContext>,
        src_host_xid: u32,
        dst_host_xid: u32,
        src_x: i16,
        src_y: i16,
        dst_x: i16,
        dst_y: i16,
        width: u16,
        height: u16,
        plane: u32,
    ) -> io::Result<()>;

    #[allow(clippy::too_many_arguments)]
    fn put_image(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        depth: u8,
        width: u16,
        height: u16,
        dst_x: i16,
        dst_y: i16,
        data: &[u8],
    ) -> io::Result<()>;

    #[allow(clippy::too_many_arguments)]
    fn get_image(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        format: u8,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        plane_mask: u32,
    ) -> io::Result<Option<Vec<u8>>>;

    /// Read a depth-1 pixmap's pixels as a tightly packed `(width,
    /// height, bytes)` triple where each byte is non-zero for set
    /// pixels and zero for clear. Used by SHAPE `Mask` to convert a
    /// mask pixmap into YX-banded rectangles. Default impl returns
    /// `None` so backends without bitmap-introspection (host-X11
    /// proxy) can stay best-effort.
    fn read_depth1_pixmap(
        &mut self,
        _origin: Option<OriginContext>,
        _host_xid: u32,
    ) -> io::Result<Option<(u32, u32, Vec<u8>)>> {
        Ok(None)
    }

    fn poly_line(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        coordinate_mode: u8,
        points: &[u8],
    ) -> io::Result<()>;

    fn poly_segment(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        segments: &[u8],
    ) -> io::Result<()>;

    fn poly_rectangle(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        rectangles: &[u8],
    ) -> io::Result<()>;

    fn poly_arc(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        arcs: &[u8],
    ) -> io::Result<()>;

    fn poly_point(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        coordinate_mode: u8,
        points: &[u8],
    ) -> io::Result<()>;

    fn poly_fill_rectangle(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        rectangles: &[u8],
    ) -> io::Result<()>;

    fn poly_fill_arc(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        arcs: &[u8],
    ) -> io::Result<()>;

    fn fill_poly(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        coord_mode: u8,
        points: &[u8],
    ) -> io::Result<()>;

    fn fill_rectangle(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
    ) -> io::Result<()>;

    fn poly_text8(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        body: &[u8],
    ) -> io::Result<()>;

    fn poly_text16(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        body: &[u8],
    ) -> io::Result<()>;

    fn image_text8(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        background: u32,
        text_len: u8,
        body: &[u8],
    ) -> io::Result<()>;

    fn image_text16(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        background: u32,
        text_len: u8,
        body: &[u8],
    ) -> io::Result<()>;

    // ──────────────────────────────────────────────────────────────
    // RENDER
    // ──────────────────────────────────────────────────────────────

    fn render_create_picture(
        &mut self,
        origin: Option<OriginContext>,
        host_drawable: AnyHandle,
        ynest_format: u32,
        value_mask: u32,
        values: &[u8],
    ) -> io::Result<Option<PictureHandle>>;

    fn render_change_picture(
        &mut self,
        origin: Option<OriginContext>,
        host_pic: u32,
        body: &[u8],
    ) -> io::Result<()>;

    fn render_free_picture(
        &mut self,
        origin: Option<OriginContext>,
        host_pic: u32,
    ) -> io::Result<()>;

    fn render_create_glyphset(
        &mut self,
        origin: Option<OriginContext>,
        ynest_format: u32,
    ) -> io::Result<Option<GlyphSetHandle>>;

    fn render_free_glyphset(
        &mut self,
        origin: Option<OriginContext>,
        host_gs: u32,
    ) -> io::Result<()>;

    fn render_add_glyphs(
        &mut self,
        origin: Option<OriginContext>,
        host_gs: u32,
        body_tail: &[u8],
    ) -> io::Result<()>;

    fn render_free_glyphs(
        &mut self,
        origin: Option<OriginContext>,
        host_gs: u32,
        glyph_ids: &[u8],
    ) -> io::Result<()>;

    #[allow(clippy::too_many_arguments)]
    fn render_composite(
        &mut self,
        origin: Option<OriginContext>,
        op: u8,
        host_src: u32,
        host_mask: u32,
        host_dst: u32,
        src_x: i16,
        src_y: i16,
        mask_x: i16,
        mask_y: i16,
        dst_x: i16,
        dst_y: i16,
        width: u16,
        height: u16,
    ) -> io::Result<()>;

    #[allow(clippy::too_many_arguments)]
    fn render_composite_glyphs(
        &mut self,
        origin: Option<OriginContext>,
        minor: u8,
        op: u8,
        host_src: u32,
        host_dst: u32,
        mask_fmt: u32,
        host_gs: u32,
        src_x: i16,
        src_y: i16,
        items: &[u8],
        x_off: i16,
        y_off: i16,
    ) -> io::Result<()>;

    fn render_fill_rectangles(
        &mut self,
        origin: Option<OriginContext>,
        host_dst: u32,
        op: u8,
        color: [u8; 8],
        rects: &[u8],
        x_off: i16,
        y_off: i16,
    ) -> io::Result<()>;

    #[allow(clippy::too_many_arguments)]
    fn render_trapezoids(
        &mut self,
        origin: Option<OriginContext>,
        op: u8,
        host_src: u32,
        host_dst: u32,
        host_mask_format: u32,
        src_x: i16,
        src_y: i16,
        traps: &[u8],
        x_off: i16,
        y_off: i16,
    ) -> io::Result<()>;

    /// RENDER `Triangles` (minor 11), `TriStrip` (12), `TriFan` (13).
    /// `primitives` is the wire body after the fixed prefix:
    /// 24-byte `XTriangle`s for minor 11, packed `XPointFixed`s
    /// (8 bytes each) for 12 and 13.
    #[allow(clippy::too_many_arguments)]
    fn render_triangles_op(
        &mut self,
        _origin: Option<OriginContext>,
        _minor: u8,
        _op: u8,
        _host_src: u32,
        _host_dst: u32,
        _host_mask_format: u32,
        _src_x: i16,
        _src_y: i16,
        _primitives: &[u8],
        _x_off: i16,
        _y_off: i16,
    ) -> io::Result<()> {
        Ok(())
    }

    fn render_create_solid_fill(
        &mut self,
        origin: Option<OriginContext>,
        color: [u8; 8],
    ) -> io::Result<Option<PictureHandle>>;

    fn render_create_linear_gradient(
        &mut self,
        origin: Option<OriginContext>,
        body: &[u8],
    ) -> io::Result<Option<PictureHandle>>;

    fn render_create_radial_gradient(
        &mut self,
        origin: Option<OriginContext>,
        body: &[u8],
    ) -> io::Result<Option<PictureHandle>>;

    fn render_create_cursor(
        &mut self,
        origin: Option<OriginContext>,
        host_src_pic: PictureHandle,
        x: u16,
        y: u16,
    ) -> io::Result<Option<CursorHandle>>;

    fn render_set_picture_clip_rectangles(
        &mut self,
        origin: Option<OriginContext>,
        host_pic: u32,
        body: &[u8],
    ) -> io::Result<()>;

    fn render_set_picture_filter(
        &mut self,
        origin: Option<OriginContext>,
        host_pic: u32,
        body: &[u8],
    ) -> io::Result<()>;

    fn render_set_picture_transform(
        &mut self,
        origin: Option<OriginContext>,
        host_pic: u32,
        body: &[u8],
    ) -> io::Result<()>;

    fn render_query_version(&mut self, origin: Option<OriginContext>) -> io::Result<(u32, u32)>;

    // ──────────────────────────────────────────────────────────────
    // DRI3 (Phase 4.2)
    // ──────────────────────────────────────────────────────────────

    /// Hand the client a render-node fd matching the scanout device.
    /// Default impl: DRI3 unsupported on this backend (HostX11Backend
    /// and `RecordingBackend` keep the default; KmsBackend overrides).
    /// On success the returned fd's ownership transfers to the caller,
    /// which dispatches it to the client via `SCM_RIGHTS`.
    ///
    /// `_drawable` is currently unused (single-GPU only — the fd is
    /// the same regardless of which drawable triggered the request).
    fn dri3_open(&mut self, _drawable: u32) -> io::Result<std::os::fd::OwnedFd> {
        Err(io::Error::other("DRI3 unsupported on this backend"))
    }

    /// DRI3 capability surface. The `(0, 0)` sentinel for `version`
    /// signals "DRI3 entirely unsupported"; the dispatcher's
    /// extension-query path filters DRI3 out of `EXTENSIONS` in that
    /// case. `modifiers`, `fence_fd`, `syncobj` are sub-capabilities
    /// that gate individual requests rather than the whole extension
    /// — see the §4 fallback matrix.
    fn dri3_capabilities(&self) -> Dri3Caps {
        Dri3Caps::unsupported()
    }

    /// Import a client-supplied dma-buf as a server-owned pixmap and
    /// return the host xid the dispatcher should bind. The `fd`
    /// ownership transfers into the backend on success; on Err the
    /// fd is dropped (closed) by the OwnedFd's normal drop path.
    ///
    /// Phase 4.2 only handles single-plane (RGB) imports; the
    /// multi-plane variant of `PixmapFromBuffers` is rejected at the
    /// dispatcher.
    #[allow(clippy::too_many_arguments)]
    fn dri3_import_pixmap(
        &mut self,
        _fd: std::os::fd::OwnedFd,
        _width: u16,
        _height: u16,
        _stride: u32,
        _offset: u32,
        _modifier: u64,
        _depth: u8,
        _bpp: u8,
    ) -> io::Result<PixmapHandle> {
        Err(io::Error::other("DRI3 import unsupported on this backend"))
    }

    /// Return `(window_modifiers, screen_modifiers)` for the
    /// `(depth, bpp)` X format under the given window. Per design
    /// §3.2: screen list is the full GPU-supported set; window list
    /// is the subset that the window's output can flip-scanout (so
    /// `window_modifiers ⊆ screen_modifiers` always holds).
    ///
    /// Default impl returns `[LINEAR]` for both lists — backends
    /// that lack `VK_EXT_image_drm_format_modifier` end up here per
    /// the design §4 fallback matrix row 1.
    fn dri3_supported_modifiers(&self, _window: u32, _depth: u8, _bpp: u8) -> (Vec<u64>, Vec<u64>) {
        // 0 == DRM_FORMAT_MOD_LINEAR.
        (vec![0], vec![0])
    }

    /// Export an existing pixmap's backing memory as a fresh dma-buf
    /// fd plus the metadata `BufferFromPixmap` reply needs. Phase 4.2
    /// design §3.2. Returns `(size, width, height, stride, depth, bpp,
    /// fd)`. The fd's ownership transfers to the caller.
    ///
    /// Default impl is unsupported.
    fn dri3_export_pixmap(
        &mut self,
        _host_xid: u32,
    ) -> io::Result<(u32, u16, u16, u16, u8, u8, std::os::fd::OwnedFd)> {
        Err(io::Error::other("DRI3 export unsupported on this backend"))
    }

    /// Import a `sync_file` fd as the backing of an XSync `Fence`
    /// resource. The fd ownership transfers into the backend; on
    /// success the backend owns a `VkSemaphore` keyed by `fence_xid`.
    ///
    /// Default impl is unsupported.
    fn dri3_fence_from_fd(&mut self, _fence_xid: u32, _fd: std::os::fd::OwnedFd) -> io::Result<()> {
        Err(io::Error::other("DRI3 FenceFromFD unsupported"))
    }

    /// Export the `VkSemaphore` backing `fence_xid` as a fresh
    /// `sync_file` fd. Returned fd's ownership transfers to the
    /// caller.
    ///
    /// Default impl is unsupported.
    fn dri3_fd_from_fence(&mut self, _fence_xid: u32) -> io::Result<std::os::fd::OwnedFd> {
        Err(io::Error::other("DRI3 FDFromFence unsupported"))
    }

    /// Import a `DRM_SYNCOBJ` fd as a timeline `VkSemaphore` keyed by
    /// `syncobj_xid`. fd ownership transfers in.
    ///
    /// Default impl is unsupported.
    fn dri3_import_syncobj(
        &mut self,
        _syncobj_xid: u32,
        _fd: std::os::fd::OwnedFd,
    ) -> io::Result<()> {
        Err(io::Error::other("DRI3 ImportSyncobj unsupported"))
    }

    /// Drop the timeline `VkSemaphore` keyed by `syncobj_xid`.
    fn dri3_free_syncobj(&mut self, _syncobj_xid: u32) -> io::Result<()> {
        Err(io::Error::other("DRI3 FreeSyncobj unsupported"))
    }

    /// Host-signal the timeline `VkSemaphore` keyed by `syncobj_xid`
    /// to `value`. Used by `PresentPixmapSynced`'s Copy path: when
    /// the synchronous CopyArea completes, signal the client's
    /// `release_syncobj` so Mesa's `vkAcquireNextImage` wakes up.
    /// Phase 4.2 design §3.3.2.
    fn dri3_signal_syncobj(&mut self, _syncobj_xid: u32, _value: u64) -> io::Result<()> {
        Err(io::Error::other("DRI3 SignalSyncobj unsupported"))
    }

    /// Trigger an XSync `Fence` resource imported via DRI3
    /// `FenceFromFD`. For xshmfence-backed fences (Mesa's
    /// loader_dri3) this calls `xshmfence_trigger`, atomically
    /// setting the shared counter and waking any process waiting on
    /// the futex. Used by `PresentPixmap`'s Copy path to signal
    /// `idle_fence` when the GPU finishes reading.
    fn dri3_trigger_fence(&mut self, _fence_xid: u32) -> io::Result<()> {
        Ok(())
    }

    /// Per-window Present capability surface. Default impl: all-false
    /// (Copy-only, no syncobj). KmsBackend overrides;
    /// `HostX11Backend` and `RecordingBackend` keep the default.
    fn present_capabilities(&self, _window: u32) -> PresentCaps {
        PresentCaps::default()
    }

    // ──────────────────────────────────────────────────────────────
    // Other extensions
    // ──────────────────────────────────────────────────────────────

    fn xkb_proxy(
        &mut self,
        origin: Option<OriginContext>,
        minor: u8,
        body: &[u8],
    ) -> io::Result<Option<Vec<u8>>>;

    fn xfixes_change_cursor_by_name(
        &mut self,
        origin: Option<OriginContext>,
        host_cursor_xid: u32,
        name_bytes: &[u8],
    ) -> io::Result<()>;

    fn set_shape_rectangles(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        kind: u8,
        rects: &[xfixes::RegionRect],
    ) -> io::Result<()>;

    // ──────────────────────────────────────────────────────────────
    // Misc
    // ──────────────────────────────────────────────────────────────

    fn warp_pointer(
        &mut self,
        origin: Option<OriginContext>,
        dst_host_xid: u32,
        dst_x: i16,
        dst_y: i16,
    ) -> io::Result<()>;

    fn query_pointer(&mut self, origin: Option<OriginContext>) -> io::Result<PointerPosition>;

    fn list_fonts_proxy(
        &mut self,
        origin: Option<OriginContext>,
        max_names: u16,
        pattern: &str,
    ) -> io::Result<Vec<u8>>;

    fn list_fonts_with_info_proxy(
        &mut self,
        origin: Option<OriginContext>,
        max_names: u16,
        pattern: &str,
    ) -> io::Result<Vec<Vec<u8>>>;

    fn get_atom_name(
        &mut self,
        origin: Option<OriginContext>,
        atom: u32,
    ) -> io::Result<Option<String>>;

    fn get_keyboard_mapping(
        &mut self,
        origin: Option<OriginContext>,
        first_keycode: u8,
        count: u8,
    ) -> io::Result<(u8, Vec<u32>)>;

    fn get_modifier_mapping(&mut self, origin: Option<OriginContext>) -> io::Result<(u8, Vec<u8>)>;
}

// Compile-time assertion that `Backend` is object-safe and that the
// `Arc<Mutex<dyn Backend>>` shape used by the hot-path call sites is
// `Send + Sync` (so worker threads can hold it).
const _: fn() = || {
    fn assert_obj_safe(_: &dyn Backend) {}
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<std::sync::Arc<std::sync::Mutex<dyn Backend>>>();
    let _ = assert_obj_safe;
};
