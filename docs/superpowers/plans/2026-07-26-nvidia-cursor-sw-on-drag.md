# NVIDIA cursor drag-stall fix — default to SW cursor on nvidia-drm

Status: **IMPLEMENTED** on branch `perf/nvidia-staging-buffer-pool`
(pushed as `perf/nvidia-staging-pool-rebased`). Needs HW verification.

## Root cause (HW-measured, GTX-1050 / nvidia-drm, XFCE Thunar drag)

`CursorPlane::move_to` → legacy `drmModeMoveCursor` **blocks ~11.5ms mean /
16.3ms max (~1 vblank), 1200× in one drag, ebusy=0** — cleanly blocking the
single-threaded loop on every cursor move. Legacy cursor move is µs on
amdgpu/Intel but blocks ~1 vblank on nvidia-drm. Second independent
discrete-NVIDIA drag blocker (besides the GetImage/rdepth1 readback the mask
cache addresses), likely the bigger one.

## Why NOT atomic cursor (rejected)

codex-reviewed AND user-confirmed from prior experience: yserver already tried
atomic cursor commits — it **"screwed up rendering (much slower)"** (the
atomic-cursor-vs-atomic-pageflip EBUSY storm; abandoned bundle-cursor-atomic
branch). A nonblock atomic cursor also tends to EBUSY behind the scanout flip →
vblank-paced cursor. So atomic is a dead end on this codebase.

## The fix (user-validated: "SW cursor made it smooth again")

On nvidia-drm, **don't use the HW cursor plane at all — use the SW (composited)
cursor.** The cursor becomes a cheap GPU quad in the composite (avg_gpu_render
~1.5ms), avoiding BOTH the legacy 11.5ms `move_cursor` block and the atomic
render regression. Empirically smooth on NVIDIA. HW cursor stays the default on
amdgpu/Intel (fast there; SW-cursor's compositor-cadence lag was the original
reason the HW plane exists — see cursor_plane.rs header).

### Implementation (~15 lines, low-risk — no DRM cursor-plane code)
- `PlatformBackend::is_nvidia_drm()` (platform.rs): `device.get_driver()` name
  contains "nvidia"; best-effort (`get_driver` err → false → keep HW cursor).
- `SceneCompositor::new` (scene.rs): `hw_cursor_strategy_enabled =
  hw_cursor_strategy_enabled() && !platform.is_nvidia_drm()`. On NVIDIA →
  `cursor_mode()` returns `Sw` → the scene composites the cursor.
- No env kill-switch added (user rule); `YSERVER_HW_CURSOR=0` still forces SW
  everywhere as before. Gate is automatic by driver.

## HW test matrix (change is cursor-visible; user tests across boxes)
- **amdgpu (RX580), Intel:** UNCHANGED — still HW cursor. Regression check
  (cursor still smooth, no compositor-cadence lag reintroduced).
- **NVIDIA (GTX-1050, +others):** cursor now SW → the `warp-perf:
  cursor_plane_move` bracket should VANISH (cursor no longer hits the plane);
  XFCE drag smooth (MATE-level); cursor tracks pointer, correct hotspot, visible
  on all outputs incl. multi-output crossing; idle desktop — moving the cursor
  still updates (SW cursor wakes a compose; watch for any idle-cursor lag).
- Watch: SW cursor after VT-switch resume (the "stale SW cursor pixels in the
  scanout BO" concern the HW plane originally fixed — verify resume is clean on NVIDIA).

## Diags to remove before merge
`warp-perf` (process_request.rs + backend.rs cursor_plane_move/pointer_fanout/
dispatch brackets, handle_warp_pointer timing), `flush-perf`, `rdepth1_diag`.

## Also on this branch (validated, landable pending Peter's shaped-window visual OK)
Mask cache (~55% rdepth1 cut) + staging pool (stable ~62KB).
