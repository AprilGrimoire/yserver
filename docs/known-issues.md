# Known Issues

Cross-cutting bugs, limitations, and dev-loop friction that surface
during validation, debugging, or interactive use. Phase-bound feature
work lives in [`status.md`](status.md) under each phase's follow-ups
section; this file is for issues that don't fit a phase or aren't
worth a phase of their own.

Add items as you find them. Tick them off when fixed. Prefer concise
entries with enough context for a future debugging session to start
from.

## Input, grabs, event routing

- [~] **GTK wheel scroll needs another app to open first (residual
      after XI2-valuator-scroll fix).** Largely fixed 2026-05-15 by
      adding XIScrollClass + scroll valuators 2/3 to the master
      pointer's XIQueryDevice reply, emitting XI_Motion events with
      cumulative scroll-axis values on each wheel click, and
      reporting the current counter as the axis's `value` field so
      mid-session clients pick up the right baseline. Caja now
      scrolls from the first click in its default view.
      Residual: the FIRST app a yserver session opens
      occasionally doesn't scroll on its initial scrolls. The
      trigger to unstick is **any scroll event reaching any GTK
      target** — scrolling on the MATE desktop (caja-managed)
      counts even though it produces no visible scroll. After
      that, scrolling works everywhere for the rest of the
      session. Non-deterministic: across three test runs, two
      needed the warm-up and one worked first try.
      Wire is delta-correct now (Relative mode, ±1 per click,
      NoEmulation flag, valuator 2/3 declared). The race appears
      to be between yserver emitting XI_Motion-with-scroll-axis
      and GDK's XI2 device-init handshake completing — if GDK
      hasn't finished caching the device's scroll classes when
      the first event arrives, the event is dropped. Real Xorg
      may avoid this by sending some initial axis state with
      XI_HierarchyChanged or by serializing XIQueryDevice replies
      against subsequent events.
      User workaround: scroll once on the desktop or in any
      other GTK app to prime. Filed for follow-up; not worth
      blocking on.
- [x] **GTK file-manager right-click popup offset + rubber-band
      anchor wrong (Caja + Thunar).** Fixed 2026-05-15 in commit
      `ea7c186`: the XIQueryPointer reply encoder placed the BOOL
      `same_screen` field in byte 1 (pad0) instead of byte 32 per
      the XI2 spec, and stuffed an extra `mask` u16 where
      `buttons_len` belongs. Every XI2 client read
      `same_screen=false`, which routes GTK's popup-placement
      through the "pointer on a different screen" branch — that
      branch treats pointer-root coords as window-local input to
      `XTranslateCoordinates`, double-adding the window's root
      origin. Pinpointed via x11trace capture (see
      `just yserver-xfce-hw-trace`). The earlier "X-only by 640"
      observation from the partial 2026-05-15 diagnosis was a
      coincidence of single-output 1920×1080 geometry where the
      panel was at the top; in dual-output 5120×1440 both axes
      were misplaced.
- [ ] **`UnmapNotify.from_configure = true` never wired.** Encoder
      accepts the byte for wire correctness; every call site currently
      passes `false`. The `true` path fires when a parent's
      `ConfigureWindow` shrinks a child out of view. Wire once we
      track parent-resize-driven implicit unmaps. (Phase 1 follow-up.)
- [ ] **`RRSelectInput` mask storage.** `RRSelectInput` is accepted
      but the mask is not stored; `RRScreenChangeNotify` is never
      delivered to RANDR clients. (Phase 2 follow-up.)
- [ ] **`GrabButton` Sync mode replay.** Current implementation stores
      the frozen event but `AllowEvents(ReplayPointer)` requires access
      to the `xid_map` which (pre-Phase-6.3) lived on the pump thread.
      Phase 6.3 moved `xid_map` into `HostX11Backend`; revisit whether
      replay is now wirable, or whether it still needs an inter-thread
      channel. (Phase 2 follow-up.)
- [ ] **`SendEvent` parent-tree propagation.** Current impl delivers
      to direct window subscribers; does not propagate up the window
      tree (`event_mask=0` with `PointerWindow` / `InputFocus`
      destinations). (Phase 2 follow-up.)
- [ ] **Input-shape hit testing in pointer pump.** Deferred since
      Phase 3.2. SHAPE input rectangles are stored locally but pointer
      hit tests don't honor them — clicks land on shape-rejected
      regions when they should pass through.
- [ ] **`CreateCursor` `XColor` struct layout.** Xlib `XColor` layout
      must match the system Xlib headers; verify on non-CachyOS target
      platforms. (Phase 2 follow-up.)
- [~] **Crossing-event `child` field + detail codes.** Partially
      fixed 2026-05-15. The protocol-level wire issues that this
      entry originally claimed (no `child` field, hardcoded
      `detail=0`) are addressed: `CrossingEvent` now has a `child`
      field, encoder writes it, and `update_pointer_window`'s chain
      walk computes proper detail codes
      (NotifyInferior/Ancestor/Virtual/Nonlinear/NonlinearVirtual)
      via `crossings::normal_mode_crossings`, plus
      `crossings::implicit_grab_crossings` now also populates
      `child` per spec.
      Original repro (e16 hover popup fires over xterm) is **not
      fixed** by these changes. x11trace analysis showed e16 IS
      selecting Enter/Leave + Motion on root, and is receiving
      Enter events with correct detail codes after the fix — but
      the popup still fires. Whatever signal e16 uses for the
      "click on desktop" hover gating isn't load-bearing on
      Enter/Leave-with-detail semantics in a way these changes
      affect. Needs a side-by-side wire-trace comparison against
      real Xorg+e16 to identify the actual signal e16 relies on
      (candidate: motion-event delivery semantics for embedded
      widgets, or a XEmbed-protocol interaction). Deferred.
      Followup gap noted during investigation: yserver's
      `update_pointer_window` only emits crossings on **top-level**
      transitions. Per X11 spec, when cursor enters a top-level
      with descendants, Enter events should fire on EACH window in
      the chain (deepest descendant → ancestors), with detail codes
      indicating position-in-chain. yserver instead fires only on
      the top-level; for now the propagation walk in
      `pointer_fanout::pointer_propagation_target_by_id` covers the
      common case (subscribers on ancestors get events from
      descendants via walk-up). This is non-spec-compliant but
      practically works for xfce/mate/marco. Re-do as deepest-target
      tracking if a real client surfaces a bug here.
- [ ] **GTK3 tree-view expander triangles don't reliably toggle.**
      Surfaced during Phase J ynest+fvwm3+gtk3-demo / ynest+e16+gtk3-demo
      smoke. After fixing two prerequisite routing bugs (the
      `PointerEvent.child` field was hard-coded to 0 → fvwm3
      MainMenu fired on every click; XI2 events were being stolen by
      core grabs → GTK never saw ButtonRelease) and the XI2
      `buttons` mask post-event semantics (was always-set on Release;
      now correctly reflects post-event button state), gtk3-demo's
      tree expanders work for *one* click — the first expand triggers,
      then subsequent collapse / expand clicks on triangles don't
      register. Trace logging confirmed the failing clicks deliver
      byte-identical XI2 events to gtk3-demo as the working click
      (same target, same xi2_targets, same coords ±1px, same `state`,
      reasonable time deltas, comfortably outside double-click
      threshold). Suspect remaining issues are XI2 wire-encoding
      details GTK is sensitive to but we don't currently emit:
      `flags` field is hard-coded to 0 (could need `XIPointerEmulated`
      or similar), `valuators_len` is 0 (GTK tree views may rely on
      X+Y valuators for hit-testing inside a column), or device-
      hierarchy / device-changed events that GTK uses to track the
      master pointer's identity. Not a regression of the
      single-threaded core — same-shaped event flow worked
      immediately for xterm, xclock, xeyes, and for gtk3-demo's main
      list-row clicks.

## Drawing / rendering artifacts

- [ ] **Per-client GC mirroring** (Phase 3.7 task #26). The shared
      host GC creates subtle bugs when GC state leaks between clients.
      Phase 3.7's fill-style fix needed careful "reset to Solid after
      each draw" to avoid tile state bleed; Phase 6.3 added similar
      careful state management for `function`/`plane_mask`/etc. via
      `apply_draw_state`. Real per-client GC mirroring would
      eliminate the reset-after-draw discipline. ~Medium scope.
- [ ] **Sub-window Expose for off-screen / behind-sibling drags.**
      Backing-store mitigation from Phase 3.6 covers the common case;
      synthetic Expose for the corner cases (window dragged fully
      off-screen / fully behind another) is the proper fix and is
      deferred. Re-open if a real validation scenario demonstrates a
      backing-store gap.
- [~] **KMS: `render_set_picture_filter` — convolution code in
      tree but unreachable; Nearest / Bilinear still share LINEAR.**
      Phase 1 (`898dd19`, 2026-05-15) parses `SetPictureFilter` +
      stores the filter on `PictureState::Drawable`. Phase 2 (in
      tree, uncommitted as of 2026-05-15) wires the `convolution`
      case through a new GPU pipeline
      (`kms/vk/convolution_pipeline.rs`,
      `docs/superpowers/plans/2026-05-15-render-convolution-filter.md`):
      builds clean, unit + integration tests pass against lavapipe.
      However, no real-world consumer reaches the path: xfwm4 /
      marco / xfwm4-shadow use pre-rendered alpha pixmaps (no
      kernel filter); picom v13 with `xrender` + kernel blur is
      the canonical consumer, but on yserver it stops drawing
      after one frame (likely Damage / XFixes infrastructure gap).
      Independent fix landed alongside: `QueryFilters` reply now
      advertises the standard X.Org filter list (was previously
      empty), so clients that consult it before sending
      `SetPictureFilter` will at least find `convolution` in the
      list. Also Nearest / Bilinear distinction is still
      collapsed to the standard pipeline's LINEAR sampler — fixing
      that needs the multi-sampler split in `RenderPipelineCache`
      called out in the original entry. Lower priority than
      finding a real consumer for the convolution path;
      rendercheck passes either way.

- [ ] **Compositor shadow margins render as opaque bars
      (NameWindowPixmap → BadAlloc).** Diagnosed 2026-05-15 from
      `xfce.xtrace`: xfwm4's built-in compositor under
      xfce4-session calls `NameWindowPixmap` on each redirected
      window (e.g. menu pop-up depth-32 183×501,
      `window=0x00d000f4`), and yserver's `name_window_pixmap`
      (`kms::backend::KmsBackend`, `backend.rs:10506`) returns
      `Err(NotFound)` → wire **BadAlloc** because
      `host_window_to_backing` is empty: the 2026-05-14 Manual-
      redirect fix (`feedback_composite_manual_redirect_trap`)
      registered the redirect record but skipped
      `activate_redirect_backing_for`, so no backing is allocated.
      xfwm4 falls back to `CreatePicture` on the **live** window,
      which only carries visible RGB — the GTK Client-Side-
      Decoration shadow-alpha that lives in the offscreen backing
      is unreachable, so xfwm4 emits opaque shadow-color pixels
      where the alpha gradient should fade. Visible as thin
      perpendicular dark bars to the right and below xfce pop-up
      menus. Same gap will affect every Manual-mode compositor
      (xfwm4, picom xrender backend, xcompmgr, compton). Fix
      shape: implement Manual-mode redirect backing properly —
      allocate at redirect activation, route window drawing into
      the backing, keep live for compositors. Half-day snapshot
      variant (lazy-alloc on `NameWindowPixmap` + one-shot copy
      from the existing window mirror) would mask the artefact
      for short-lived popups but lies about live updates.
- [ ] **KMS: `MapSubwindows` doesn't re-Expose deep descendants
      promoted by the map_window viewable cascade.** After commit
      `304858f` (`fix(resources): propagate Viewable down through
      Unviewable descendants on map`), mapping a window correctly
      flips deep descendants from `Unviewable` to `Viewable`.
      `handle_map_window` already calls
      `emit_expose_subtree_to_state` which walks descendants, so
      that path is fine. But `handle_map_subwindows` only emits
      MapNotify+Expose for the children it directly maps, not for
      their grandchildren that may have just been promoted by the
      cascade. Edge case (most clients only have one level of
      MapSubwindows children) but spec-incorrect. Fix: in
      `handle_map_subwindows`, after the per-child loop call
      `emit_expose_subtree_to_state` on each child whose viewability
      transitioned. ~10 LoC.
- [ ] **Damage accumulation on RENDER drawing ops.** Phase 3.5's
      first-cut `accumulate_damage` covers core drawing only.
      RENDER-driven damage (composite, fill rectangles, glyphs) is
      not accumulated. Matters once a real client (compositor /
      screen recorder) drives the path.
- [x] **~~Background image colors look R↔B swapped.~~ FIXED
      2026-05-14.** yserver advertises X.Org-standard masks
      (`red=0x00FF0000`, `green=0x0000FF00`, `blue=0x000000FF`) and
      echoes the client's byte_order as `image_byte_order`, so the
      spec-correct LE wire encoding of a ZPixmap pixel is `[B,G,R,A]`
      — already matching the mirror's `B8G8R8A8_UNORM` layout.
      `try_vk_put_image` was reading the wire as `[r,g,b,a]` and
      permuting to `[b,g,r,a]`, double-swapping any spec-compliant
      client. Fix: straight `copy_nonoverlapping` for depth-32; same
      for depth-24 with a `byte[3]:=0xFF` post-pass to keep the
      mirror opaque for RENDER composites. Confirmed on fuji: MATE
      wallpaper + Chrome icon now render correctly. Test fixture
      doc + the three PutImage/CopyArea alpha_invariant inputs
      updated to feed wire bytes in spec order.
- [x] **Text rendering broken under xfce4 / GTK heavy workloads.**
      Fixed by Phase 3E text-run migration + downstream rework;
      confirmed working on fuji 2026-05-15.

## wmaker on KMS

- [ ] **wmaker title-bar close/minimize button glyphs missing.**
      Same general area as the appicon (CWBackPixmap-driven small
      icons) but the depth-1 PutImage fix didn't cover it. The
      buttons render as plain coloured 25×25 squares without their
      X / − glyphs. Probably a different drawing path — wmaker may
      be using PolySegment / PolyLine to stroke the glyphs after a
      ClearArea, and either the glyph strokes are clipped or our
      drawing primitives don't honour something they need. Cosmetic;
      drag/move/close-via-menu still work.
      wmaker comes up on KMS and draws its dock/clip; xterm under
      wmaker connects, sends MapRequest, gets reparented into a
      wmaker frame, gets configured to fit, and renders text into
      its (now-hidden) backing pixmap — but the frame itself is
      never mapped, so xterm is invisible.

      Trace from client 0 (wmaker) shows the typical sequence:
      `GrabServer` → frame `CreateWindow` → frame-children
      `CreateWindow` + `MapWindow` → long restack chain across all
      existing top-levels → frame `ConfigureWindow` (geometry + border
      width) → `ReparentWindow` xterm into frame → `ConfigureWindow`
      xterm to fit. After that point wmaker continues drawing
      decorations and creating an appicon, but no `MapWindow` is
      ever sent on the frame xid (0x10051b in the captured run) and
      no `MapWindow` on xterm's own xid follows the reparent.

      wmaker also logs `internal X error: 0 Request code: 0 DUMMY
      Request minor code: 2 Resource ID: 0xff000000 Error serial:
      10172` early — the resource id `0xff000000` doesn't match any
      xid yserver allocates and `code: 0` isn't a real X11 error
      code, so wmaker is either mis-parsing one of our replies or
      reacting to something our error encoder produced with a wrong
      `code`/`major_opcode`/`bad_value` triple. That error happens
      well before xterm starts, so it may or may not be related to
      the missing `MapWindow`.

      fvwm3 (`yserver-fvwm3-xterm` recipe) is fully working;
      wmaker's flow has different expectations that we don't meet.

      **Diff against ynest (where wmaker+xterm works):** in ynest,
      wmaker's MapRequest handling continues past frame creation to:
      ```
      CreatePixmap 64x64 (appicon)
      MIT-SHM::PutImage (upload appicon image)
      MIT-SHM::Detach
      ChangeSaveSet (on the xterm window)
      MapWindow frame
      MapWindow xterm
      ```
      In yserver, wmaker stops before any of these — the `ChangeSaveSet`
      / `MapWindow frame` / `MapWindow xterm` lines are entirely absent
      from the trace, and the appicon MIT-SHM upload for the new
      window never happens (MIT-SHM count: ynest 430, yserver 409).
      Combined with the early `internal X error: 0 ... Error serial:
      10172` wmaker logs to its catchXError handler, the most likely
      story is that we're sending wmaker a malformed error reply
      somewhere during frame setup, and wmaker's XSetErrorHandler
      branches into a "skip mapping" recovery path. Bisect candidates:
      the long restack chain (~150 ConfigureWindow with stack_mode +
      sibling), one of the SHAPE::SelectInput / SHAPE::QueryExtents
      probes, or a CWCursor / CWBorderPixmap attribute we silently
      drop in change_subwindow_attributes.

## KMS backend (Phase 6.4 / 6.5)

Surfaced while bringing up xeyes / xterm / xclock and fvwm3 against
`KmsBackend`. The backend is the bare-metal counterpart to
`HostX11Backend` — primitives go straight to a pixman scanout buffer
instead of a host X server, so gaps in our rasterisation surface here
that the host hides for us.

- [ ] **`poly_arc` / `poly_fill_arc` partial-angle clipping.** Both
      treat any arc as a full ellipse regardless of `angle1`/`angle2`.
      Fine for xeyes (full circles) but anything that draws actual
      pie slices renders as full discs. Add an angular mask: for each
      candidate pixel, check `atan2(py - cy, px - cx)` against
      `[angle1, angle1 + angle2)` (with X11's "0 = 3 o'clock,
      counter-clockwise" convention).
- [ ] **`poly_arc` outline only handles full ellipses.** Same root
      cause as above — the cap/connector logic doesn't know about
      partial arcs. Once angle clipping is in, the outline algorithm
      needs the same treatment plus proper arc endpoints (so a
      half-arc outline doesn't close itself across the chord).
- [ ] **`poly_line` thick lines.** GC `line_width` ignored; we always
      rasterise as 1-pixel Bresenham. Most clients use line_width=0
      (server-discretion thin) but anything wanting a 3- or 5-px line
      would render too thin.
- [ ] **Opcode 58 (SetDashes) unsupported.** Logged as
      `unsupported opcode 58` from fvwm modules; means dashed lines
      aren't honoured.  Cosmetic — dashes fall back to solid.
- [ ] **Opcode 81 (InstallColormap) unsupported.** fvwm3 calls it once.
      Safe to ignore on a TrueColor backend; could just reply "did it"
      to silence the unsupported-opcode log.
- [ ] **Can't switch VT while yserver is running.** The startup
      `KDSKBMODE=K_OFF` blocks the kernel from interpreting
      Ctrl+Alt+Fn as a VT switch — it's the same mechanism that
      stops keystrokes from leaking to the underlying TTY. yserver
      currently doesn't implement the standard
      `VT_PROCESS` / `VT_RELDISP` cooperative-VT-switch protocol
      (Xorg's `xf86OpenConsole` / `xf86VTSwitch`), so even if the
      keys reached us we'd have nowhere to dispatch them. Workaround
      from a remote shell: `sudo chvt N`, or `sudo systemctl restart
      display-manager` to land back in the original session.
      Fix: install a SIGUSR1/SIGUSR2 handler bound via
      `VT_SETMODE` and release/acquire the DRM master in lock-step
      with the kernel-driven switch. Mostly cosmetic until the
      multi-server use case (X+yserver in parallel) becomes real.
- [ ] **Crash before `console::Drop` leaves the host TTY unusable.**
      yserver's startup grabs the VT with `KDSKBMODE=K_OFF +
      KD_GRAPHICS` (kernel keystroke→TTY translation suppressed) and
      relies on the matching restore in `console::Drop` for cleanup.
      Any abnormal exit that bypasses `Drop` — SEGV/SIGABRT, an
      unhandled signal whose default action is "terminate"
      (e.g. SIGUSR1 on a binary without our handler), DPMS-induced
      power-save with the box still alive, etc. — leaves the kernel
      in K_OFF on that VT. From then on Ctrl+Alt+Fn does nothing,
      the VT can't be switched away from, and the screen looks
      frozen even though the box is responsive over SSH. Recovery
      from a remote shell: `sudo systemctl restart display-manager`
      (gdm/sddm/lightdm) — clears the state and gets you back to
      Wayland/Xorg login. Hardening: install a SIGSEGV/SIGABRT
      signal handler that explicitly calls the console restore
      before re-raising, and propagate the K_OFF restore through
      panic hooks too.
- [x] **~~P0: KMS teardown leaves DRM state that breaks Wayland host
      sessions.~~ FIXED via failure-path disarm (2026-05-13).**
      Diagnosis was correct: yserver's `disable_output` left
      framebuffers bound to CRTCs that the host Wayland compositor
      (labwc/dms/Sway) then couldn't recover. Fix landed via the KMS
      teardown plan
      (`docs/superpowers/plans/2026-05-13-kms-teardown-fix.md`,
      results at
      `docs/superpowers/plans/2026-05-13-kms-teardown-fix-results.md`):
      6-step shutdown sequence + per-output `disarm` paths on
      `ScanoutBo` and `drm::buffer::Buffer` so failed atomic disables
      no longer cascade into `destroy_framebuffer` on KMS-held FBs.
      Hardware-validated: dms+labwc session recovers cleanly when
      user switches back to F1 after running yserver on F3. Kernel
      `atomic remove_fb failed with -22` WARN is gone.
- [ ] **Atomic `disable_output` returns EINVAL on AMD Polaris
      (residual bug after the P0 fix).**
      `crates/yserver/src/drm/modeset.rs:387` builds a single atomic
      commit that clears plane FB_ID/CRTC_ID, sets CRTC ACTIVE/MODE_ID
      to 0, and clears connector CRTC_ID. The kernel rejects this
      with `-EINVAL` on both DP-1 and HDMI-A-1. The teardown disarm
      path makes this harmless (failed outputs leak their FBs instead
      of being destroyed, so dms recovers), but the warning still
      fires on every shutdown and the leaked handles persist until
      DRM-fd close at process exit. Two likely causes per codex's
      original diagnosis:
      1. `MODE_ID = 0` on a blob property may need the property
         unset specifically rather than set to integer 0.
      2. Some drivers want the plane cleared in a separate atomic
         commit BEFORE the CRTC is deactivated.

      Fix recipe: split the single atomic commit into per-stage
      commits — (a) clear plane FB_ID + CRTC_ID, (b) deactivate
      CRTC + clear MODE_ID blob, (c) clear connector CRTC_ID. Each
      with `ALLOW_MODESET`. Cross-reference Mesa libdrm fallback or
      Xorg's `drmModeAtomicCommit` deactivation sequence. Medium
      priority — no user-visible regression today; cleaner shutdown
      logs + zero-leak success path are the goals.
- [ ] **xeyes-on-e16 window drag is sluggish on bare HW.** Observed
      on a real DP-attached AMD card via `yserver-e16-...-hw` (KMS).
      During a drag the dragged frame visibly lags the cursor.
      `yserver-hw.log` over the drag window shows the compositor
      saturating vblank (pageflip-complete + composite resubmit
      pairs back-to-back at ~60 Hz) but only ~20 `ConfigureWindow`/s
      reaching the WM despite continuous pointer motion — input
      delivery, e16 throttling, or composite wall-time per frame
      could each be the bottleneck and the log alone doesn't
      distinguish them. To diagnose: add timestamped tracing on
      (libinput motion → MotionNotify dispatched) and (composite
      start → flip submitted) for one drag, then correlate. The
      compositor never falls behind vblank in the log, so the most
      suspicious link is input-event delivery rate.

- [ ] **mate-control-center under adapta-nokto: catastrophic mouse
      lag.** Observed 2026-05-13 on `bee`. The moment mate-cc is
      open under the adapta-nokto GTK theme, the whole desktop becomes
      unusable — Alt-F2 takes minutes to surface, cursor lags by
      minutes behind the actual input. `amdgpu_top` shows continuous
      GPU work, so this is real submission saturation, not yserver
      spinning on CPU. Closing mate-cc (or switching to the default
      Menta theme) restores responsiveness — under default theme
      mate-cc is mostly OK, only briefly laggy. Reproduces on `master`
      (predates Phase 3F-1), so this is not a 3F-1 regression. The
      hardware angle is unconfirmed — adapta-nokto was not tested on
      `silence`, so we don't yet know whether the cliff is
      hardware-class-dependent or just absolute theme-workload-driven.

      **Post-3F-2 update (2026-05-13)**: 3F-2 retired
      `try_vk_render_traps_or_tris`'s per-call `vkQueueWaitIdle`
      (the traps half of the original two-part hypothesis), but the
      lag is **unchanged** — adapta-nokto + mate-cc still brings the
      machine down to the point where `amdgpu_top` (a separate process,
      unrelated to yserver) stops redrawing. That confirms the
      bottleneck is GPU-submission saturation, not input-loop
      starvation, which in turn rules in the remaining hypothesis:
      `GlyphAtlas::intern`'s per-glyph `queue_wait_idle` is the
      dominant remaining cost (each new glyph drains the queue
      completely before the next can be uploaded). Phase 5 scope.

      **Post-Phase-5 / pool / GPU-trap update (2026-05-15)**: Phase 5
      sync rework, the pixmap pool, and GPU trap rasterization all
      landed; perceived lag on fuji (Intel) improved dramatically.
      `bee` (AMD RDNA2) is "no GPU faults but laggy" per status.md.
      `silence` (powerful machine with discrete GPU) **still
      reproduces the catastrophic mate-cc + adapta-nokto cliff**,
      which rules OUT "the slow machine just can't keep up" — the
      absolute submission count or per-frame work exceeds the
      budget even on capable hardware. Remaining suspects:
      `GlyphAtlas::intern` per-glyph wait still present (filed
      separately); RENDER Composite path's per-op submission rate
      under heavy GTK-glyph workloads; or a higher-level frame
      management gap that lets one workload starve subsequent ones.
      Cross-vendor reproduction (Intel-fuji OK, AMD-bee residual,
      AMD-silence catastrophic) suggests hardware/driver factors
      compound on top of an absolute-rate problem.

## WM-specific behaviour

- [ ] **e16 popup rounded corners.** Cosmetic — popup outer shape is
      Set+Intersect rectangular bounding; rounded look comes from
      bg-pixmap content, with small black pixels at the very corners
      because the popup outer's bg isn't auto-filled there
      (default bg = None). Investigate `ParentRelative` bg forwarding.
- [ ] **openbox frame chrome.** Frame title bars / labels / buttons
      don't draw under openbox even after the Phase 3.4 atom fix;
      clients render correctly inside the frames. Suspect: openbox
      draws frame decorations into 1×1 sub-windows of the frame and
      that drawing path doesn't reach the host (same family as the
      old wmaker chrome bugs).

## Extension polish

- [x] **~~Composite extension: xfwm4 / picom / xcompmgr can't
      redirect (`update=1` Manual mode rejected with BadValue).~~
      FIXED 2026-05-14.** Surfaced as the MATE notification-area
      applet crashing on every login (Gdk error handler aborts on
      `BadValue, request_code 144 (Composite), serial 282`).
      Fix shape per the previous "register the record but skip
      `activate_redirect_backing_for`" recommendation: both
      `update=0` (Automatic) and `update=1` (Manual) now insert the
      redirect record, but the backing-pixmap activation is dropped
      from all three call sites (composite handler x2 + CreateWindow
      child-of-redirected hook). NameWindowPixmap reads the record;
      no consumer reads the backing today. `activate_redirect_backing_for`
      kept under `#[allow(dead_code)]` for revival when the
      backing-as-source compositor path lands. Confirmed on fuji:
      notification-area-applet survives login.
- [x] **~~RANDR `SetCrtcConfig` / `SetScreenConfig` return
      `BadValue=0` instead of a Success reply.~~ STUB-FIXED
      2026-05-15.** Surfaced once the `minor_code` threading fix
      made RANDR error replies legible: mate-settings-daemon's
      display-settings restore on login was failing because the
      dispatcher hard-rejected RANDR minors 2 (SetScreenConfig)
      and 21 (SetCrtcConfig) with BadValue. yserver runs at a
      fixed KMS-set mode and doesn't reconfigure outputs/CRTCs
      on demand, so the handler now stubs a Success reply with a
      current timestamp. The screen stays at the active mode (fine
      for the single-mode-per-session use case); the client thinks
      its restore worked. MATE's "couldn't save/restore display
      settings" warning is gone.
- [ ] **Real RANDR `SetCrtcConfig` / `SetScreenConfig` (runtime
      mode change).** The stub above always replies Success
      without actually switching modes. To honour the client's
      request we'd need ~2–3 phases of work spanning the protocol
      and KMS backend:
      1. Enumerate all KMS-supported modes per connector via
         `drmModeGetConnector` and surface them through
         `GetScreenResources` / `GetOutputInfo` with stable XIDs.
      2. Round-trip mode XID → `drmModeModeInfo` for atomic
         commit.
      3. Runtime modeset entry point in `kms::backend` (we have
         startup `enable_output` + `disable_output`; need a
         "switch this CRTC to mode M" that re-allocates scanout
         BOs at the new size, reprograms the atomic commit, and
         rolls back on kernel rejection).
      4. Resize composite pass attachments + Vulkan framebuffers;
         drain in-flight work cleanly across the switch.
      5. Resize the X11 root, fire ConfigureNotify/ResizeRequest
         to top-levels, recompute the window-tree layout against
         the new screen extent.
      6. Send `RRCrtcChangeNotify` + `RRScreenChangeNotify` to
         every RANDR client that selected input (depends on the
         separate "RRSelectInput mask storage" followup
         landing first).
      7. Multi-output sequencing + partial-failure semantics when
         the client configures multiple CRTCs in one request flow.
      8. Wire-level validation: reject mode XIDs not ours, reject
         kernel-rejected configurations.
      Each piece is reasonable on its own; together it touches the
      compositor critical path and is its own phase of work.
      Deferred until the use case (interactive resolution change /
      hot-plug reconfiguration) becomes load-bearing.
- [ ] **e16 RENDER coverage audit.** Was deferred in Phase 3.4 because
      e16 didn't reach a stable rendering state. Phase 3.4's atom-name
      fix unblocked e16 startup, so this audit is now actionable. Run
      e16 for ~60s, capture all RENDER opcodes touched, compare against
      the implemented set in status.md's RENDER section.
- [ ] **xclock "Missing charsets in String to FontSet conversion"
      warning at startup.** Benign and matches stock Xorg behaviour
      without CJK bitmap fonts installed — libXt walks the locale's
      full charset wishlist (jisx*/gb2312*/ksc*/big5*/various
      iso8859-N) and warns for those it can't load, then proceeds
      with whatever it found (iso8859-1 + iso10646-1). Fix would be
      to probe each font's `FcCharSet` in `build_font_catalog` and
      emit jisx*/gb2312*/ksc*/big5 entries when a real CJK font is
      installed. Defer until a CJK rendering client matters.
## Core loop fairness

- [~] **Listener accept starves under high-volume per-client request
      streams.** Not currently reproducible (as of 2026-05-15) but
      the unbounded per-iteration read budget that underlies it is
      still present, so the shape remains. Original observation
      from 2026-05-13 with `just yserver-xfce-hw`:
      xfce4-panel made 5 attempts to open `:7.0` over ~50ms, all
      returned "cannot open display"; yserver-hw.log shows zero new
      client setups between 13:00:48 (client 38) and the user's
      Ctrl-Alt-Backspace zap at 13:00:59, despite the kernel-side
      `connect()` succeeding for those 5 panel attempts.
      Meanwhile client 13 (likely xfdesktop or xfsettingsd) sent
      **32,293 QueryPointer requests in 11 seconds** (~3000/sec) and
      client 6 (xfwm4) was hammering SHAPE::Combine. The single-threaded
      core loop's per-iteration work was dominated by reading existing
      clients' floods; mio readiness for the LISTENER_TOKEN was queued
      but never serviced, so the new fds languished without an X11
      setup handshake completing.
      Root cause: `core_loop::run_core`'s mio poll iteration likely
      drains each ready fd until WouldBlock before checking the next
      one (a classic head-of-line blocking pattern). Fix shape: cap
      per-client read budget per poll iteration (e.g., 16-32 requests
      per client per tick) so a chatty client doesn't monopolize the
      core. Alternatively, prioritize LISTENER_TOKEN at the top of the
      iteration. Touches `core_loop/run.rs`; medium scope.
      Secondary observation: client 13's 3000 Hz QueryPointer polling
      is itself worth investigating — that's likely yserver returning
      a stale or wrong reply to xfdesktop's pointer query, causing it
      to retry indefinitely. Spec compliance issue.

## Dev-loop / observability

- [x] **~~X11 error encoder hard-codes `minor_code = 0` for
      extension errors.~~ FIXED 2026-05-15.** Threaded the minor
      opcode through 76 extension-dispatcher call sites in
      `process_request.rs` via the existing
      `emit_x11_error_with_minor` helper. Composite, MIT-SHM (+
      children: CreatePixmap, PutImage, GetImage, CreateSegment),
      PRESENT, DRI3, GLX, RANDR, XI2/XKEYBOARD, RENDER all now emit
      error replies with the real per-extension minor. Core
      requests stay on `emit_x11_error` with `minor=0` (spec-correct
      for non-extension errors). Future emit_x11_error log lines
      decode the failing minor immediately.
- [ ] **README pointer to this file.** So the next reader knows
      where the bug ticklist lives.

## Archived: ynest-era (pre-rendering-rework, KMS-direct supersedes)

yserver now runs KMS-direct (no host X server). The items below
were filed when yserver ran nested under `ynest` against the
host Xorg; many of the failure modes simply don't exist on the
KMS path. Kept here for reference if we ever revive ynest-based
WM smoke testing.

- [ ] **CopyArea BadMatch (~100/wmaker startup).** ynest passed
      `CopyArea` requests through to the host X server, which
      rejected drawable pairs of mismatching depth. Pre-rework
      noise from wmaker on ynest.
- [ ] **PolyFillRectangle BadMatch (~15/wmaker startup).** Same
      shape as the CopyArea pattern (ynest forwarded; host
      rejected).
- [ ] **Composite NameWindowPixmap BadAccess (~28/wmaker
      startup).** ynest called `NameWindowPixmap` on un-redirected
      windows; host rejected with BadAccess. Mattered for
      picom-like compositors under the old setup.
- [ ] **MIT-SHM `XShmPutImage` host fast path.** Would have let
      ynest forward a single 256-KB+ image to the host without
      chunking. Irrelevant on the KMS path (no host server).
- [ ] **gtk3-demo / gtk4 demo silent-exits.** gtk-demo started
      and exited silently against the host `:0`, nested or not.
      Pre-existing dconf/stdin quirk in the dev environment.
- [ ] **Down-grade known-benign host errors to DEBUG.** Small
      classifier in `BackendEventSink::handle_backend_event` to
      drop the WARN noise from ynest-host error replies. No host
      server on the KMS path.
- [ ] **fvwm3 segfaults on host container resize / window close.**
      Triggered by ynest-host window resize. yserver KMS doesn't
      currently support output resize at all, so this code path is
      not reachable. Pre-existing fvwm3 bug independent of yserver.
- [ ] **fvwm: apps disappear after host container resize.** Same
      ynest-host-resize trigger as the segfault above. Not
      reachable on KMS-direct.
