# picom on yserver — MSC idle-starvation: diagnosis + spike + handoff

**Branch:** `feat/present-vblank-msc-spike` (off `master`, this commit).
**Date:** 2026-06-26. **HW:** bee (AMD 6900HX / RADV, amdgpu atomic KMS).
**Status:** root cause fully confirmed; partial fix (MSC clock) implemented + validated; the remaining piece (idle vblank arming) is specified below, NOT yet done.

---

## Symptom

Run picom on yserver (`just startx` → awesome → `picom --backend glx`). Windows
go blank / only repaint when you click the desktop or drag another window over
them; btop never animates on its own; content "never drawn correctly."

## Root cause (CONFIRMED) — Present MSC idle-starvation

picom's `present` vblank scheduler paces every repaint on `PresentNotifyMSC`:
it asks "wake me at MSC N", repaints when the `CompleteNotify` arrives, then
asks for N+1. **master never advances MSC**, so picom gets one tick and freezes.

Proven by Xorg+x11trace diff (`awesome-xorg.xtrace`, display :99), same
awesome+picom+btop workload:

| signal | Xorg (works) | yserver master | yserver (this spike) |
|---|---|---|---|
| NotifyMSC requests | **781** | 1 | many (clock runs) |
| Present CompleteNotify | 894 | ~1 | flows |
| DAMAGE-Notify | 157 | ~12 | ~12 |
| DAMAGE-Subtract | 157 | **0** | 0 |

### The "damage problem" was a RED HERRING
It looked like a damage re-arm bug (0 Subtract vs Xorg's 157). It is **not**.
picom binds window content fine on yserver (this run: 14 `NameWindowPixmap` +
15 DRI3 `BuffersFromPixmap`) and gets the initial per-window DamageNotify. It
never enters the repaint→subtract loop because of a **bootstrap deadlock**:

> The spike advances MSC only on **pageflips**. picom redirected *every*
> window → yserver's scene is just the static overlay → yserver doesn't flip
> → MSC never advances → picom's NotifyMSC never completes → picom never
> presents → no flip. Chicken-and-egg.

Confirmed by pageflip cadence in `yserver-hw-startx.log`: bursty/event-correlated
(54–60 flips in the seconds you manually damaged a window, 1–4 in quiet seconds)
— **never steady 60/s**. picom only "sort of animates" when your input forces a
flip that ticks the clock once.

**Conclusion: `drmCrtcQueueSequence` idle-arming is MANDATORY**, not optional.
It ticks MSC every vblank when nothing is flipping, breaking the deadlock. The
active-screen-only assumption was wrong for a full-screen compositor. Legacy
`drmWaitVBlank` will NOT substitute — serviced ~1s late under amdgpu atomic
(that was the original cinnamon-keyring symptom on the source branch).

---

## What this spike already does (DONE, builds clean, validated)

Minimal "drive MSC from real pageflips" — the part-(a) of the real fix.
Result: picom's clock is clean (`Invalid PresentCompleteNotify` 0, `did not
complete during vblank` 118→11). Necessary, not sufficient (needs idle-arming).

Files changed:
- `crates/yserver/src/drm/page_flip.rs` — `dispatch_event`/`drain_events`
  widened to forward `PageFlipEvent.frame` (msc) + `.duration` (ust).
- `crates/yserver/src/kms/v2/platform.rs` — `PlatformBackend.ust_msc`
  (per-output `(msc,ust)`), captured in `drain_page_flip_events` (now `&mut`),
  exposed via `present_get_ust_msc()`.
- `crates/yserver-core/src/backend/trait_def.rs` — `Backend::present_get_ust_msc()`
  default `(0,0)`.
- `crates/yserver/src/kms/v2/backend.rs` — impl forwards to platform.
- `crates/yserver-core/src/server.rs` — `PendingNotifyMsc`, ServerState
  `present_pending_msc` + `present_kernel_{msc,ust}`.
- `crates/yserver-core/src/core_loop/process_request.rs` — NOTIFY_MSC handler
  parks unsatisfied requests (was: dropped); `fire_present_notify_msc_complete_events`
  takes a real `ust`; new `fire_due_present_notify_msc(state, msc, ust)`.
- `crates/yserver-core/src/core_loop/run.rs` — `drain_present_completions`
  mirrors backend `(msc,ust)` into ServerState and fires due NotifyMSC.

---

## What's LEFT (the fix) — port idle vblank arming from the source branch

Source: `origin/fix/present-vblank-msc-rebase` (codex-converged design in
`docs/superpowers/specs/2026-06-01-idle-vblank-msc-pacing-design.md`). DO NOT
whole-merge that branch — it bundles stale T9/COW work
(`arm_cow_from_recent_present_if_needed`) that diverges from master's evolved COW.

Port these commits' code (adapt to master, reference only):
- `e3254467` — `DRM_IOCTL_CRTC_QUEUE_SEQUENCE` consts + `drm_crtc_queue_sequence`
  / `drm_event_crtc_sequence` structs (`page_flip.rs`). Copy VERBATIM (ioctl
  number + struct layout are pinned by unit tests `*_struct_is_{24,32}_bytes`).
- `44bc27b5` — `queue_crtc_sequence(device, crtc_id, relative, sequence, user_data)`
  raw-ioctl helper. crtc_id is the RAW KMS object id, NOT a pipe index.
- `16fa9119` — decode `DRM_EVENT_CRTC_SEQUENCE` (arrives as `Event::Unknown`)
  in `dispatch_event`. Branch uses a 2-callback `drain_events(on_advance,
  on_sequence)`; this spike collapsed to one `(crtc,msc,ust)` cb — either adopt
  the branch's 2-cb shape, or route on_sequence into the same `ust_msc` update.
- `e485470c` + `bf8e7570` + `7a955843` — arm idle vblanks; per-CRTC armed-target
  map (replaces a single bool — needed for multi-monitor); reconcile (clear arm)
  on suspend / DPMS-off / output removal. **Invariant (load-bearing): every path
  that drops/completes an armed sequence MUST clear its armed-map entry, else
  that CRTC stalls at ~0fps.**

### Wiring (minimal spike version)
1. When `state.present_pending_msc` is non-empty after `drain_present_completions`
   and no flip is imminent, call a new `Backend::arm_idle_vblank()` →
   `queue_crtc_sequence(primary_crtc_id, relative=true, sequence=1,
   user_data=crtc_id)`. Track armed state so you don't re-arm every iteration.
2. The armed vblank fires → `DRM_EVENT_CRTC_SEQUENCE` on the DRM fd →
   `drain_page_flip_events` (`on_sequence`) updates `ust_msc` + clears armed.
3. Next `drain_present_completions` fires due NotifyMSC + re-arms if still pending.
EOPNOTSUPP (pre-4.14): fall back to relative `drmWaitVBlank` keep-alive (branch
Task 8) — bee's kernel supports the ioctl, so optional for the spike.

---

## How to test (bee, TTY)
```
just startx                      # rebuilds; awesome via ~/.xinitrc
# from awesome:
picom --backend glx --log-level=debug --log-file=picom-glx-spike.log
# open btop — SUCCESS = animates continuously on its own, no clicking.
```
Damage trace (if needed): `just startx log="warn,yserver_core::core_loop::damage_fanout=trace"`
then grep `yserver-hw-startx.log` for `damage_fanout … match_ids/fired_count`.

## Gotchas / dead-ends (don't relearn these)
- Use **glx** backend (master has TFP). xrender survives but does NOT use the
  `present` scheduler, so the MSC fix can't help it.
- UST must be **real CLOCK_MONOTONIC micros** — picom rejects `ust=0`
  ("Invalid PresentCompleteNotify event, <msc> 0"). The spike reads it from
  `PageFlipEvent.duration`; idle-arming reads `drm_event_crtc_sequence.time_ns`.
- xtrace WORD-SWAPS Present CARD64 (msc/ust look like garbage / 2^32). Trust
  counts & cadence, not literal 64-bit values.
- A 1ms wall-clock kick exhausts picom's XIDs (~28s) — no XC-MISC on master.
  Don't pace faster than vblank.
- "startx → console" earlier was a `.xinitrc` mistake, NOT auth. yserver xauth is fine.
- Then HARDEN: tests (deferred NotifyMSC fires on simulated vblank advance;
  armed-map clear on suspend/output-removal), run past codex, decide merge.

## Memory
`~/.claude/.../memory/project_picom_compositor_diagnosis.md` has the condensed version.
