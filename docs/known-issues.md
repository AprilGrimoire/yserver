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
- [~] **Cinnamon click-to-focus replay path was incomplete on the
      XI2 side.** Surfaced 2026-05-26 while validating
      `cinnamon-settings`: the settings window activated on click,
      but the clicked row/button never received the press. Trace
      logs showed the focus-grab owner immediately sending
      `XIAllowEvents(deviceid=2, mode=ReplayDevice)` after each
      activation click. The first fix wired ReplayDevice through the
      core replay helper, but the deeper bug was that
      `XIPassiveGrabDevice` had been recorded with `pointer_mode=1`
      unconditionally, so muffin's sync passive grabs never froze the
      ButtonPress in the first place. yserver therefore leaked the
      XI2 press to the unfocused target before focus activation and
      had nothing useful to replay afterward. Fixed in the current
      tree by preserving XI2 passive-grab modes, withholding the
      initial XI2 ButtonPress from non-grab-owner clients when the
      grab is synchronous, and replaying the XI2 press to the natural
      target on `XIAllowEvents(mode=2)`. Added a regression test that
      models the full sequence. The earlier attached-slave XI2
      topology cleanup remains useful hardening for Cinnamon/GTK, but
      it was not the direct cause of the dead clicks. Follow-up trace
      data exposed a second bug in the focus path: once muffin
      focused the toplevel, `cinnamon-settings` moved focus into an
      inferior child window and yserver reported that as a generic
      top-level focus loss (`FocusOut detail=NotifyAncestor`) instead
      of the correct ancestor-chain transition (`NotifyInferior` on
      the parent, `NotifyAncestor` on the child). Cinnamon then
      cleared `_NET_ACTIVE_WINDOW`, so the focused window looked like
      it was deactivating under the pointer on every click. Current
      tree fixes both core and XI2 focus fanout to use real
      ancestor-chain details and keeps the child as the active
      keyboard focus; regression coverage was added for the
      top-level -> child move. A follow-up working Xorg `x11trace`
      showed the remaining click stream now matches closely on the
      wire, so routing/replay/focus are no longer the obvious
      blockers. The clearest remaining mismatch was protocol
      negotiation: yserver still advertised XI 2.2 while Xorg
      negotiated XI 2.4 for the same clients. Current tree now
      negotiates `XIQueryVersion` like Xorg for a 2.4-capable
      server, with regression coverage for 2.4/2.3 replies. Another
      Xorg diff then exposed a second metadata gap: GTK selects
      `XI_DeviceChangedMask` on the root window and Xorg immediately
      sends a master-pointer `XI_DeviceChanged` event with labeled
      button/valuator classes; yserver had been sending none, and its
      `XIQueryDevice` pointer classes were still unlabeled. Current
      tree now bootstraps that `XI_DeviceChanged` event and labels the
      virtual pointer classes with Xorg-style atoms. Build/test
      validation is green.

      **Cinnamon runtime re-test 2026-05-26 PM — input path now
      verified good, symptom moved.** Captured one-click trace of
      `cinnamon-settings applets` clicking on the Themes category.
      The XI2 press reaches `cinnamon-settings` cleanly (client 052
      in the xtrace, window 0x3500006, single press + single release
      at the correct coords). GTK's click handler runs through to
      completion on the app side: `GrabServer` / `XIQueryPointer`×3 /
      `UngrabServer` / `GetInputFocus` / `XIChangeCursor`, then
      `ChangeProperty WM_NAME = "System Settings > Themes"`, then 7×
      `MIT-SHM::Attach` for the new panel's buffers. muffin observes
      the property change and repaints the decoration — the user
      sees the **title bar update to "System Settings > Themes"**.
      But `cinnamon-settings` issues no further draw calls after the
      SHM attaches — no `PutImage`, no `RENDER::Composite`, no
      `CopyArea` to window 0x3500006. The user sees the **categories
      grid still rendered**; only the title changed. Symptom is
      identical with a 2 s post-click wait and with a longer wait,
      so it is not just panel-load latency.

      Conclusion: **this is not a yserver input / focus / protocol
      bug anymore**. cinnamon-settings's Python click handler is
      hanging between "title updated + SHM attached" and "draw new
      panel". Suspects (none confirmed): a blocked DBus call from
      the panel-load code (color-manager, accountsservice, polkit),
      a slow `Gtk.IconTheme` / theme enumeration, a GSettings read
      that hangs because the underlying daemon misbehaves under
      yserver. Open follow-up: capture a Python stack via
      `py-spy dump --pid <cinnamon-settings-pid>` (or
      `strace -p <pid> -e trace=recvmsg,poll` for the syscall view)
      while the broken state is on screen. That will name the
      blocked frame and we can decide whether it's a yserver-visible
      bug after all (e.g. a missing event the daemon waits on) or
      strictly a Cinnamon-side issue worth handing back to Cinnamon
      upstream. Until then, downgrade priority — MATE is the
      validated desktop, Cinnamon's click activation works on the
      wire and the next step is process-side diagnosis.
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
## Drawing / rendering artifacts

- [ ] **nemo desktop rubber-band selection renders a stepped
      (staircase) leading edge during the drag (Cinnamon, bee,
      2026-05-27).** Exposed once the rubber-band anchored correctly
      (after the `XIGetClientPointer` deviceid fix, `967c09d`). While
      dragging a marquee selection on the nemo desktop, the selection
      rectangle's trailing/top/left edges are clean but the **leading
      (right/bottom) edge is a diagonal staircase of translucent
      steps** — old band edges from prior frames remain visible. **On
      button release it briefly shows the correct, clean rectangle**
      before the band clears.
      Evidence (captured drawable dump
      `yserver-v2-drawable-0-present-src-0-*.ppm`, this is the
      composited scanout source): the staircase is in the composited
      output itself.
      Diagnosis: NOT geometry and NOT the client's output. nemo
      double-buffers — it renders desktop+band into a backing pixmap
      and `CopyArea`s the band bbox to the desktop window each frame
      (trace shows large per-frame copies, e.g.
      `dst=(7,0) 627x496`), so nemo's source is a clean rectangle. The
      "correct on release" behaviour proves the backing is correct and a
      **full** repaint renders it right; only the **incremental** repaint
      during the drag is wrong. This points at v2's Stage 2e buffer-age
      clipped repaint: with 2-3 scanout buffers, each buffer must repaint
      the damage accumulated since *it* was last drawn, not just the
      latest frame's sliver. If the moving band's window-content damage
      is only applied for the current frame, the buffer retains band
      pixels from ~2 frames ago in the uncovered gap — and that
      stale edge is exactly one "step". Release issues a full-region
      redraw, so every buffer gets clean content → correct briefly.
      So far **nemo-specific**: caja (MATE, same nautilus rubber-band
      code) does not show it on yserver — possibly because caja's band
      is opaque/XOR rather than a translucent overlay, so stale pixels
      don't read as a visible step. Worth confirming caja doesn't
      staircase under close inspection.
      Next step: trace the v2 damage path for a moving sub-region —
      confirm whether window-content damage from `CopyArea` / RENDER
      fills is accumulated across buffer generations in
      `pick_repaint_region` / the `BufferAgeRing`, or only applied to the
      current frame. Fix is almost certainly in that accumulation, not
      in the input/protocol layer.

- [ ] **wezterm client content briefly blank after heavy drag
      stress (yoga / Snapdragon X1 / Turnip, release build,
      observed once 2026-05-19 PM).** During a deliberate
      window-drag stress test (purpose: check whether v2 spikes
      CPU under marco-comp drag — it doesn't), wezterm's client
      area went blank/white for a few seconds while marco's frame
      decoration kept rendering normally. Resolved on its own.
      Not reproducible on a debug build (timing-sensitive). No
      telemetry capture at the moment of the symptom.
      Two working hypotheses:
      1. Buffer-age history loss during the drag-induced
         ConfigureNotify storm + Stage 2e failed-flip recovery
         momentarily presenting a stale/uninit BO for wezterm's
         backing.
      2. Marco briefly dropping its `NameWindowPixmap` alias on
         wezterm mid-drag, so its compositor reads nothing for a
         few frames and the COW shows marco's frame-fill (white)
         through where wezterm content should be.
      Reproduction hint: heavy drag of any window while wezterm is
      running with btop or similar continuously-painting workload.
      Capture `yserver-mate-hw-telemetry` log during a repro
      attempt — diff per-second counters across the symptom
      window for clues (especially `composite_submit`,
      `frame_present`, scene-structure damage path).

- [ ] **Stage 4c follow-up: multi-output coord-space for
      scene-structure damage rects.** `SceneCompositor::
      mark_scene_structure_damage_rects` (4c.1, scene.rs:410)
      clips each input rect by `output_extent` only, assuming
      every output starts at origin (0, 0). Stage 4c.4 feeds it
      screen-absolute rects from `window_absolute_rect`
      (backend.rs:558). Under single-output (layout_x0 = layout_y0
      = 0) screen-absolute ≡ output-local, so the clip is correct.
      Under multi-output deployments with non-(0,0) layouts the
      helper would clip in the wrong frame and a damage rect on
      output 2+ would either be dropped (rect outside its
      extent-from-origin) or land at the wrong screen offset.
      Either: (a) add `layout_origin: vk::Offset2D` to
      `OutputSceneState` and threading through `SceneCompositor::
      new`, or (b) translate at the call site in
      `set_window_scene_participation`. Not a regression vs the
      4c.1 stub (same shape), but undocumented before this entry
      and tracked here to surface when multi-output work begins.

- [ ] **Stage 4c follow-up: `src_size = [1, 1]` after redirected W
      resizes past B's extent.** `build_scene`'s redirect-aware
      indirection (Stage 4c.3, commit `b9a9f3a`) emits
      `CompositeDraw.src_size = [1.0, 1.0]` (normalized UV "sample
      whole texture") for the W entry. When W is Automatic-redirected
      to backing B and W later RESIZES past B's extent, the scene
      stretches B onto W's new larger `dst_size` instead of leaving
      the now-uncovered region undefined. Conversely on W-shrink the
      scene samples a B-region larger than W now is, with the same
      undefined result. Either: (a) resize B in lockstep with W (per
      Composite spec NameWindowPixmap aliases freeze at pre-resize
      content so this is delicate), or (b) clamp `src_size` to the
      W/B extent ratio. Plan §4c didn't pin a behaviour; revisit
      before the 4c hardware smoke gate (mate + marco) or risk
      stretched window content on resize.

- [ ] **xeyes resize-DOWN artefact on v2 + mate + marco
      (2026-05-17, open).** Continuous-drag-shrink leaves xeyes
      with eye geometry sized for a *wider* window than what's
      now displayed — eye 2 visibly cut off at the right edge.
      Resize-UP works clean after the 2026-05-17 fix chain
      (MaskScratch swizzle + trap-shader AA + xid-detach +
      pixmap zero-fill). Two hypotheses, neither confirmed:
      1. xeyes' eye-geometry state stale during rapid drag —
         pixmap dims caught up but draw geometry didn't. Verify
         by stopping the drag and waiting 2-3 seconds before
         dumping the scanout; if the eyes settle to correct
         smaller shape, it's an xeyes-side race.
      2. v2 scene compositor blits the wrong storage — perhaps
         a pending-ack carrying an old DrawableId. Less likely
         given the test coverage, but worth re-checking the
         `pending_acks` capture path.
      Also visible (orthogonal): many `render_composite gap:
      host_src 0x40xxxx not resolvable` lines from marco's
      decoration compositing, depending on `name_window_pixmap`
      stubbed `Err` on v2 (Stage 4). Real fix is Stage 4 — the
      gaps aren't a v2 regression, but the noise complicates
      diagnosis here.
- [~] **MATE panel flicker on v2 (2026-05-17).** Reported with the
      resize-down session above. **Likely resolved 2026-06-01** by the
      systray ClipByChildren fix below — the speculated "rapid panel
      applet activity" cause matches the notification-area damage storm
      (~485 FillRectangles/socket per vsync), which recomposited the
      panel strip every frame. Re-confirm on a MATE session; if flicker
      persists outside the tray region it's a separate scene-damage
      issue.

- [x] **Systray damage storm + invisible tray icons — FIXED
      (2026-06-01, branch `clip-by-children`).** mate-panel's
      notification area drove a per-vsync loop (~485 FillRectangles per
      tray socket; 1486 DamageNotify vs Xorg's 29) and tray icons never
      rendered, on every machine. Single root cause: yserver never
      applied `ClipByChildren` to RENDER paint or its damage. A tray
      socket is fully covered by its mapped XEMBED icon child, so the
      applet's `FillRectangles op=Clear` on the socket is a no-op in
      Xorg (empty composite clip → no paint, no damage). yserver painted
      + damaged the full socket → wiped the icon backing (invisible
      icons) and fired the socket's own damage object → panel
      recomposites → clears → loop. Fixed at the RENDER FillRectangles
      site (`process_request.rs` opcode 26): clip both the paint
      (`clip_fill_rects_by_children`, subtract mapped InputOutput
      children, empty → skip op) and the damage
      (`accumulate_damage_clip_by_children_to_state`); shared
      `mapped_child_clip_rects` helper in `damage_fanout.rs`.
      HW-verified on silence: icons visible, loop dead, ~half the
      session's protocol traffic eliminated. **Follow-up:** the same
      ClipByChildren gap remains on the other RENDER paint sites
      (Composite, CompositeGlyphs, Trapezoids/Triangles) — not
      tray-exercised, but the same latent class. See `status.md`
      "Systray applet loop — RESOLVED".

- [ ] **Tray icons intermittently blank / bottom-sliver — manual-redirect
      paint-routing race (2026-06-01, open).** Distinct from the storm fix
      above: after `e7a1ba0` icons *can* render, but intermittently come
      up blank or as a thin bottom line; varies run-to-run (1–3 icons
      affected). `YSERVER_TRAY_DEBUG=1` proved it's NOT geometry/clip —
      socket 26×27, plug child 26×27@0,0 Viewable, backing 26×27, and the
      panel's Clear is correctly a no-op (`clipped=[]`). The icon is blank
      because the plug's paint doesn't land in the socket's redirect
      backing *that the panel reads*. The initial paint-routing-race
      hypothesis was **refuted** by probe (`yserver::kms::v2::paint=trace`:
      `NO_REDIRECT_FOUND=0`, `REDIRECT_FOUND=116` — plug paints DO route to
      a backing). So capture works; the bug is **downstream in the
      read/composite of the backing**. Leading candidates: (1) **source-read
      asymmetry** — the panel reads each socket as a Composite SOURCE; if
      that samples the live (empty) socket not the backing → blank. This is
      what the parked `fix/applet` `resolve_source_picture` redirect-walk
      fix addressed (it "didn't fix icons" then only because the storm was
      still wiping; revisit NOW on top of `e7a1ba0`). (2) **backing rotate**
      — sockets alloc `1×1` then grow `26×27`; if rotate doesn't recapture,
      a read around it could yield artifacts. NOTE (user, 2026-06-01): the
      failing icon is **fully blank**, NOT a clipped/sliver icon — a
      correctly-drawn applet has no bottom line. The "1px bottom line" seen
      in failures is a SEPARATE minor artifact (stray composited row /
      edge), not a partial icon. Blank-not-partial favors candidate (1)
      (source-read returns empty live socket) over (2). scene_walk
      `skip_reason="no_intersect_with_output"` decisions also worth
      auditing. NEXT: rebase parked `fix/applet`
      `resolve_source_picture` onto `clip-by-children` and re-test;
      instrument source-read + backing-rotate if not. Fragile/black-screen-
      prone subsystem — HW smoke before commit. Banked traces:
      `mate-working.xtrace` (all 3), `mate-sliver.xtrace` (failing). Diag
      (`YSERVER_TRAY_DEBUG`) in working tree, marked REMOVE.

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
- [ ] **`render_set_picture_filter` honoured only as Nearest.**
      v1: `render_set_picture_filter`
      (`crates/yserver/src/kms/backend.rs:12392`) accepts the
      request but ignores the filter — Vk Composite shader uses
      a fixed sampler. v2: filter is stored on
      `PictureRecord::Drawable` (3b) but the engine still
      samples Nearest regardless of the requested filter
      (`Bilinear` / `Convolution` deferred per spec § "Out of
      scope" — Stage 5 perf work). Clients that depend on
      `Bilinear` for picture-source scaling see Nearest output.
      Stage 3 status quo per the Stage 3 plan §"Non-goals" #8.

- [ ] **Compositor shadow margins render as opaque bars
      (NameWindowPixmap → BadAlloc).** Diagnosed 2026-05-15 from
      a fresh `xfce.xtrace`. Visible as thin perpendicular dark
      bars to the right and below xfce pop-up menus (any GTK
      Client-Side-Decoration popup with alpha-shadow margins).
      Same gap will affect every Manual-mode compositor: xfwm4,
      picom's xrender backend, xcompmgr, compton.

      **Five-step fault chain:**

      1. xfwm4 issues `RedirectSubwindows update=Manual(0x01)` on
         root. Accepted by the 2026-05-14 fix
         (`feedback_composite_manual_redirect_trap`).
      2. That fix registered the redirect *record* but **skipped**
         `activate_redirect_backing_for`, so
         `host_window_to_backing` stays empty for every redirected
         window.
      3. xfwm4 issues `NameWindowPixmap` for the menu (depth-32,
         override-redirect).
      4. yserver's `name_window_pixmap`
         (`kms::backend::KmsBackend`, `backend.rs:10506`) looks up
         `host_window_to_backing`, finds nothing → returns
         `Err(NotFound)` → wire-level **BadAlloc** on
         `Composite-Request(144, 6)`.
      5. xfwm4 falls back to `CreatePicture` directly on the
         **live window** instead of its offscreen pixmap, then
         composites from that picture. The live window only
         carries visible RGB; the GTK CSD shadow-alpha that lives
         in the offscreen backing is unreachable, so xfwm4 emits
         opaque shadow-color pixels where the alpha gradient
         should fade.

      **Status:** the 2026-05-15 attempt to fix this in the
      current rendering model (the abandoned
      `render-convolution-filter` branch, T1-T4 of the Manual-
      redirect plan) made the BadAlloc go away at the protocol
      level but exposed the deeper structural mismatch — yserver's
      scanout walks per-window mirrors and never displays root's
      mirror, so the compositor's RENDER paint to root is
      invisible. **Deferred** to the v2 rendering-model rewrite,
      where COMPOSITE redirect collapses to "this window's storage
      target moves" and the compositor's paint to root is
      naturally what `SceneCompositor` consumes. See
      `docs/superpowers/specs/rendering-model-v2.md` (TBD).
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
- [ ] **PictFormat tracking + ARGB picture intent (Stage 4e
      candidate).** Surfaced as residual from Stage 4 close
      (2026-05-21). v2 forces opaque on depth-24 source pictures
      (`d20f279`) and routes xRGB/RGB24 through the resolver as
      forced-opaque, but doesn't track per-picture PictFormat
      semantics (xRGB-as-render-intent vs ARGB) end-to-end. Full
      compositor-WM correctness (consistent ARGB-aware shadow /
      transparency / multi-layer Render::Composite) needs
      PictFormat tracking on `PictureRecord::Drawable` + the
      backend sampler, plus picture-source alpha interpretation
      per format. Until then the cow-authoritative scene gating
      keeps the compositor's path working but full Render
      Composite correctness in complex DE chrome (e.g. tooltip
      drop-shadows blending against arbitrary backgrounds) is
      not guaranteed. Marked candidate "Stage 4e" in earlier
      planning; not on the Stage 5 perf path.

      **2026-05-22 investigation status — UNRESOLVED yserver bug:**
      gtk3-demo + tooltip CSD shadows render visibly different on
      yserver than on Xorg-native (denser dark band at the window
      edge). Setup: **same machine, same MATE installation, same
      theme/wallpaper files, same GTK/Cairo binaries** — only the
      X server differs. Pixel diff against the Xorg reference:
      inner-shadow RGB `(31,59,48)` on yserver vs `(42,81,68)`
      on Xorg over wallpaper RGB `(75,144,117)` vs
      `(75,144,121)` — implied stored α ≈ 151 on yserver vs
      ≈ 112 on Xorg. **Even the wallpaper green channel differs
      by 4 LSB despite identical wallpaper file**, indicating a
      color-pixel-rendering divergence in addition to the shadow
      one. Per `AGENTS.md`: yserver must match Xorg, this is a
      real yserver bug, root cause not yet identified.

      Ruled out so far:
      1. Pixel-level compose math: marco's `XRenderComposite(Over,
         src=ARGB32_window_picture, mask=None, dst=offscreen_d24)`
         produces `result.rgb = src.rgb + (1-src.a)*dst.rgb`
         exactly — scanout = marco offscreen bit-for-bit. So the
         divergence is upstream of marco's compose.
      2. Force-opaque intent: `picture_pict_format` returns
         `RENDER_FMT_ARGB32` for the GTK CSD picture →
         `resolve_force_opaque_pict_format` returns false → α
         is not stomped to 1.0. Not an ARGB-vs-xRGB picture
         intent bug.
      3. Marco shadow plugin: `window_has_shadow()`
         (`compositor-xrender.c:1013-1088`) returns FALSE for
         `cw->mode == WINDOW_ARGB` with no marco frame on either
         Xorg or yserver — neither adds a Gaussian soft halo
         around CSD windows. Asymmetry vs SSD MATE apps is
         marco's intentional policy, identical on both servers.
      4. RANDR / SETUP physical-mm dimensions: confirmed
         bit-identical GTK CSD backing alpha before and after
         fixing SETUP mm (38-DPI → 96-DPI synth) and per-output
         mm (96-DPI synth → real EDID via `drmModeConnector.size()`).
         GTK does not read RANDR or SETUP mm to compute CSD
         shadow density on this path. Both fixes shipped
         2026-05-22 as a separate correctness win, see
         [feedback_dpi_hardcoded_matters.md].

      Candidates not yet ruled out — where the divergence likely
      lives in yserver:
      - **Visual / PictFormat advertisement shape** — yserver
        advertises ~5 visuals vs Xorg's ~272 (visible in
        `xdpyinfo`). GTK may pick a different visual for the
        CSD pixmap or root window based on what's listed, and
        the chosen visual's bit/mask layout drives Cairo's
        rendering path.
      - **`PutImage` byte-order / depth-32 handling** — the
        wallpaper RGB `B` channel differs by 4 LSB
        (`75,144,117` vs `75,144,121`) despite the same source
        file. This is a real color divergence in something
        upstream of the CSD shadow problem. Could be wire-byte
        permutation in `put_image` for depth-24/32, a swizzle
        in DRI3 import, or the root storage's interpretation
        of client-uploaded bytes.
      - **Cairo Xlib surface selection** — GTK calls
        `cairo_xlib_surface_create_with_xrender_format` which
        picks a format based on the server's `QueryPictFormats`
        reply shape. yserver's format-list ordering or content
        could be steering Cairo into a different rendering
        path.
      - **`RENDER QueryFilters` reply** — affects which Cairo
        filtering paths GTK uses (Nearest/Bilinear/Convolution).
        Need to confirm yserver advertises identical filter set
        to Xorg.
      - **XSETTINGS** — `mate-settings-daemon` is the same
        binary on both servers, but its initial-write may
        depend on what it reads from the server first. Compare
        the `_XSETTINGS_SETTINGS` property between sessions.
      - **`gdk-window-scaling-factor` setting source** — could
        yserver end up reporting something that makes GdkWindow
        compute a different scale?

      Next concrete diagnostic step: capture a side-by-side
      gtk3-demo run on Xwayland (96-DPI synth, no real EDID,
      same as yserver's fallback path) versus yserver, same
      session, same theme, same wallpaper file. If Xwayland's
      gtk3-demo matches Xorg-native, the divergence is yserver-
      specific (and not "Xwayland-style 96-DPI" causing it). If
      Xwayland matches yserver, the divergence is in something
      yserver-and-Xwayland both lack vs Xorg-native (likely
      something EDID-derived beyond mm dimensions, or color/
      gamma path).

      Marked candidate "Stage 4e" in earlier planning; not on
      the Stage 5 perf path.
- [ ] **KmsCore.pictures disconnect cleanup.** Stale Picture
      records from disconnected clients (e.g.
      mate-session-check) can persist in v2's `KmsCore.pictures`,
      causing `render_composite gap` noise lines.
      `yserver-core/src/core_loop/process_disconnect.rs:261`
      already loops `removed.freed_pictures` calling
      `backend.render_free_picture`; verify v2's impl actually
      evicts from `KmsCore.pictures` and add the missing eviction
      if it doesn't. ~50 LoC. Surfaced from Stage 4d open items
      (now in
      [`status-archive-2026-05-21.md`](status-archive-2026-05-21.md)).
- [ ] **MATE Control Center yellow group-header labels missing
      (mate-no-comp).** Group headers in the Control Center
      sidebar ("Filter", "Groups", "Common Tasks") render
      invisible. Reproducible without a compositor, so not a
      COW/redirect bug. Likely a colored-source + glyph-mask
      Render::Composite path issue — yserver's Render glyph
      compositing with a non-default source colour is the most
      plausible suspect. Surfaced from the Stage 4d hardware
      smoke (now archived).

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
- [ ] **Clean MATE logout leaves the console wedged.** Surfaced
      2026-05-26 on M2 Asahi (Apple Silicon, AGX-V) and reproduced
      on silence (i9 13900k + RX580 / GCN4 / RADV). Workflow: launch
      MATE via `just yserver-mate-hw…`, log out via the MATE session
      menu (clean session-end, not a zap). Expected: yserver
      `Drop` runs, console returns. Observed: screen stays on the
      last yserver frame, **`chvt` hangs** when issued from SSH,
      `sudo kbd_mode -a` does not restore input, only a reboot
      recovers. `dmesg` is clean (no DRM/AGX fence timeouts, no
      `*ERROR*`), so the GPU pipeline is not wedged — the failure
      is in userspace VT state. Both affected machines have
      vendor-modern DRM drivers (Apple AGX kernel + Mesa AGX-V on
      one, amdgpu/RADV/GCN4 on the other), suggesting it's not a
      single-vendor quirk; more likely `console::Drop` /
      `disable_output` is not getting all the way through when the
      caller exits via the session-end path. Hypothesis to verify:
      mate-session's terminating yserver doesn't go through the
      signalfd `Message::Shutdown` drain (so `disable_output` /
      `ConsoleGuard::Drop` partial-fires), OR fbcon was unbound at
      startup and isn't being rebound on this exit path. Repro is
      stable on both machines so a single diagnostic run with
      strace + `/sys/class/vtconsole/vtcon*/bind` capture
      pre/post-exit should pin it down. Recovery for now:
      SysRq SAK (`Alt+SysRq+K`) kills the wedged session on the
      current VT and rebuilds the console — works on silence
      (standard PC keyboard with a SysRq key). On the MacBook (no
      dedicated SysRq, the Fn-key remapping fights the combo), SAK
      is impractical and the only recovery is `sudo systemctl
      reboot` from SSH. Distinct from the SIGSEGV / SIGABRT
      case above — this is a *clean* session-end, not a crash.
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
- [ ] **Vulkan validation error: `shaderDemoteToHelperInvocation`
      SPIR-V capability declared but device feature not enabled.**
      Surfaced 2026-05-31 during the MIT-SCREEN-SAVER smoke run on
      bee (AMD RDNA2): `vkCreateShaderModule()` validation reports
      `SPIR-V Capability DemoteToHelperInvocation was declared, but
      one of [...] VkPhysicalDeviceVulkan13Features::
      shaderDemoteToHelperInvocation [...] is required`. yserver
      passes Vulkan-1.3 features but does not query and enable
      `shader_demote_to_helper_invocation`, while at least one
      shader (saver-overlay-related path triggers it; possibly any
      with `discard;`) declares the SPIR-V capability. Validation-
      only on bee's driver (RADV happily creates the module),
      but a stricter ICD would reject the module. Fix in
      `crates/yserver/src/kms/vk/device.rs` — query
      `PhysicalDeviceVulkan13Features::shader_demote_to_helper_invocation`,
      enable it on the logical-device create when supported, and
      log a warning + fall back to a non-demoting shader variant
      when not. User has a WIP stash with this work already in
      progress on `feature/frame-builder-submit-rate`.
- [x] **~~1471 leaked Vulkan objects on yserver shutdown.~~ FIXED
      2026-05-31 across three commits on `master`.** Bee/MATE
      vkDestroyDevice warning went from 1471 → 0 across the chain:
      `6464ef7` retained `Arc<VkContext>` on `FenceTicketInner` so
      its fallback Drop destroys the fence handle directly when
      the pool is already gone (root cause: `KmsBackendV2`'s field
      declaration ran `platform`'s `fence_pool` drop before
      `store` / `engine` / `scene` released their tickets, so the
      `Weak::upgrade() == None` branch leaked every fence — 523
      VkFence leaks); `288a6aa` added `DrawableStore::
      shutdown_destroy_all` called from lib.rs's explicit shutdown
      block so resident drawables' VkImage / VkImageView /
      VkDeviceMemory are destroyed via `Storage::destroy` (58 more
      leaks gone); `a517c0e` drained `RenderEngine::drawable_view_cache`
      in `Drop` (890 leaks gone — the bulk). Verified across four
      vkdebug recipe runs: 1471 → 948 (after fence) → 890 (after
      store) → 0 (after engine cache).
- [ ] **`vkCmdBeginRendering()` READ_AFTER_WRITE hazard on
      `pColorAttachments[0]` with `LOAD_OP_LOAD`.** Validation
      reports the LoadOp read of the attachment view races a
      preceding `vkCmdPipelineBarrier2` layout transition: the
      barrier's `dst_stage_mask`/`dst_access_mask` doesn't cover
      `COLOR_ATTACHMENT_OUTPUT` + `COLOR_ATTACHMENT_READ`, so the
      LoadOp read happens before the layout transition's write is
      visible. Observed 2026-05-31 on bee/MATE in 6 distinct
      image views during the saver-smoke run. No visible
      rendering glitches reported, but synchronisation-layer
      validation flags it; under a stricter driver the load
      could return undefined data. Fix shape: at any
      `vkCmdBeginRendering` that uses `LOAD_OP_LOAD`, ensure the
      preceding barrier into the colour-attachment-optimal layout
      includes `dst_stage_mask=COLOR_ATTACHMENT_OUTPUT` and
      `dst_access_mask=COLOR_ATTACHMENT_READ|COLOR_ATTACHMENT_WRITE`
      (currently it's likely WRITE-only). Investigate
      `crates/yserver/src/kms/v2/store.rs::transition_layout_for_dynamic_rendering`
      and the LoadOp-emitting sites in `engine.rs` /
      `scene.rs`.
- [ ] **`vkQueueSubmit2()` reads VkImage in `UNDEFINED` layout
      where shader expects `SHADER_READ_ONLY_OPTIMAL`.** Observed
      2026-05-31 on bee/MATE: `command buffer ... expects VkImage
      ... (aspectMask = COLOR, mip = 0, layer = 0) to be in layout
      VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL — instead, current
      layout is VK_IMAGE_LAYOUT_UNDEFINED`. First-use-before-
      transition: a fresh allocation (likely from a pool-take or
      freshly-allocated drawable) is bound as a sampler input
      before any barrier transitions it from the initial
      UNDEFINED. Possibly related to the
      READ_AFTER_WRITE hazard above (same code path).
      Fix shape: walk the sampler-binding sites in `engine.rs`
      / `scene.rs`; ensure every sampler bind is preceded by a
      layout-transition barrier to `SHADER_READ_ONLY_OPTIMAL`
      from `current_layout` (tracked on `Storage`).
- [ ] **`vkGetImageSubresourceLayout()` uses wrong aspect mask
      for DRM-format-modifier images.** Validation: `pSubresource
      ->aspectMask (VK_IMAGE_ASPECT_COLOR_BIT) must be
      VK_IMAGE_ASPECT_MEMORY_PLANE_i_BIT_EXT.` Observed
      2026-05-31. Per the Vulkan DRM-format-modifier ext spec,
      queries against an image created with a DRM format modifier
      must use the per-memory-plane aspect masks
      (`MEMORY_PLANE_{0,1,2,3}_BIT_EXT`), not the generic colour
      aspect. yserver's call site (likely the DRI3-import /
      scanout-BO query path in `kms/vk/target.rs` or `kms/vk/
      scanout.rs`) needs to detect the modifier and switch aspect
      mask. Functionally probably fine on RADV (returns plausible
      layout for COLOR aspect on single-plane modifiers) but
      formally non-conforming and could break on other ICDs or
      multi-plane formats.
- [ ] **`RenderEngine::notify_drawable_retired` is defined but
      never called.** `engine.rs:2507` exposes a per-drawable
      cache-invalidation hook for `drawable_view_cache`, but a
      tree-wide grep shows zero callers. The shutdown leak
      (`a517c0e` above) only drained the cache at SIGTERM; at
      runtime every `DestroyPixmap` / `DestroyWindow` leaves the
      drawable's cached views (one per `(SamplerConfig,
      SwizzleClass)` triple) in the cache, referencing the
      destroyed image. Long sessions accumulate dead entries
      until process exit. Not a kernel leak (cleanup at exit),
      but unbounded growth and the cached views point at freed
      images. Fix shape: call `engine.notify_drawable_retired(id)`
      from `DrawableStore::destroy_now` (which already owns the
      destroy moment), threaded through `KmsBackendV2`'s
      destroy-window / destroy-pixmap entry points. Touches the
      runtime path so worth small TDD coverage.

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

## Window lifecycle

- [ ] **Top-level window vanishes mid-session while the X client
      process stays alive.** First observed 2026-05-30 ~00:00 local on
      bee/yserver-hw under MATE: an alacritty terminal disappeared
      from screen while `ps auxf` still showed the alacritty
      process, and a `DumpDrawables` dump confirmed the window was
      gone from yserver's tree (not just unmapped — absent
      entirely). Marco kept compositing fine (the cat wallpaper +
      a newly-spawned terminal rendered to the scanout), so this
      is a per-window teardown, not a compositor stall. The bug
      was not triggered by deliberate user action — happened
      spontaneously during a long-running `cargo build`.
      Root cause **unknown** — the active log was `RUST_LOG=warn`
      (`Justfile:469` at the time) and produced no warn-level
      trace: no `BadWindow`, no `BadDrawable`, no destroy/
      disconnect lines. The only client-level warn in the window
      was 9× `client 14 CreatePixmap BadIDChoice pid=0xffffffff
      (out-of-range)` at 22:06:31 UTC, but the timing analysis put
      alacritty's vanish before the first warn line (21:56:47, a
      VT-switch-recovery EACCES burst), so client 14 is almost
      certainly the debug terminal spawned after the bug — not
      alacritty.
      Captured artefacts (saved 2026-05-30 ~00:09 local):
      `yserver-hw-mate.log`, `yserver-v2-drawable-0-windows.txt`,
      `yserver-v2-scanout-0-out0.ppm`, the 32 `present-src-*.ppm`
      ring dumps, and `scanout-0.png`.
      Next-time setup: `Justfile:466` now defaults the
      `yserver-mate-hw` recipe's `RUST_LOG` to include
      `yserver_core::core_loop::pointer_fanout=trace,
      yserver_core::core_loop::process_request=debug` — useful but
      doesn't cover client lifecycle. For the next repro also
      raise `yserver_core::core_loop::process_disconnect=debug`
      (currently logs `process_disconnect: client X close_mode=Y`
      at debug level — `process_disconnect.rs:92`) and the
      `client_reader` module so the EOF / I/O-error path that
      drops a client's fd is visible. Cheapest follow-up if a
      grep-level trace is enough: promote
      `process_disconnect::process_disconnect`'s entry log
      from `debug!` to `warn!` so future warn-only captures pin
      the disconnect timing.
      Open hypothesis space (none yet refuted):
      a) alacritty's X connection broke (e.g. EPIPE on a write)
         → yserver fired `Message::ClientDisconnected` →
         `process_disconnect` destroyed its windows; alacritty
         survived because its main loop doesn't exit on X EOF.
      b) marco issued `KillClient` or `DestroyWindow` against
         alacritty for reasons unknown.
      c) alacritty itself sent `DestroyWindow` from a recovery
         path triggered by an earlier (silent) error.
- [ ] **BadWindow storm on screensaver/dialog teardown.** Observed
      2026-05-31 on bee/MATE during a clean screensaver
      activate → blank → wake → unlock cycle: ~10–15 `BadWindow
      (code=3)` errors land on marco (client 11) and a secondary
      MATE component (client 14) within ~2s of each saver-window
      DestroyWindow. The bad xids (`0x3f00004`, `0x1000129`,
      `0x10000e`) are the just-destroyed saver/dialog windows;
      the failing requests are GetProperty (opcode 20),
      GetGeometry (14), GetWindowAttributes (3), ChangeProperty
      (18), ReparentWindow (7), CirculateWindow (19). marco/MATE
      have requests queued against the window between yserver's
      DestroyNotify dispatch and the clients draining their
      stream. Cosmetic in logs and functionally invisible (the
      X spec requires the server to error in this case and the
      client to handle the error), but the volume is high
      enough to suggest yserver could either delay DestroyNotify
      until clients ACK their queues (Xorg doesn't), or recognise
      the racey pattern and demote to a single rollup line.
      Lower priority: real Xorg has the same race, just with
      different timing characteristics that hide it most of the
      time. No fix required unless a future investigation finds
      a downstream effect.

## Extension polish

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

- [ ] **README pointer to this file.** So the next reader knows
      where the bug ticklist lives.
- [ ] **yserver stalls under a headless virtio-gpu with no display
      (`-display none`).** Surfaced 2026-05-26 while bringing up xts
      `Xlib9` in vng: the `xts-yserver` recipe used
      `-device virtio-gpu-pci -display none`, under which KMS atomic
      pageflips never get a completion event. yserver's compose path
      gates on flip-pending state (Stage 2f per-output gate), so with
      no flip-done the scene loop intermittently wedges — clients
      drawing to windows hang mid-test. Symptom in xts was a
      deterministic stall a few test-cases in, then `Could not open
      display :7` ABORTs for every later case (the wedged compose
      starves new-client setup). Switching the recipe to the Venus
      `egl-headless,gl=on` display config (a working flip path)
      removed it: full `Xlib9` went from 4 cases + 1375 ABORT to 18
      cases + 0 ABORT in the same time budget. The recipe is fixed;
      the underlying robustness gap remains — yserver should not
      livelock when pageflip completions never arrive. A
      pageflip-timeout / degraded-present path in the compose loop
      would harden the headless / broken-KMS case. Was repeatedly
      misread as a "depth-4 GetImage hang" because the depth-4
      `UnsupportedDepth` warnings were the last thing logged before
      each stall; the depth-4 path itself is fast (verified offscreen
      under lavapipe) and unrelated.

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
