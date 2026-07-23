# Handoff: XI2 grab-ownership desync → GTK dialog input wedge

Branch: `wip/xi2-grab-ownership-desync` (analysis only, no code changes yet).
Found while triaging the remaining items from discussion #96 (digitaltrails' report).

## Symptom
In a GTK dialog under **Cinnamon**, clicking a widget that takes a pointer/keyboard
grab wedges the whole dialog's input — a **combobox** popup (xfce-terminal
Preferences → Scrolling → scrollback) *and* a plain **checkbox** both do it. The
WM decoration (titlebar) still works, because muffin is a separate client and only
the app's client is starved.

## Repro
Cinnamon session, open `xfce-terminal` → Preferences → click the scrollback combobox
(or a checkbox) → input wedges. Recover with Ctrl-Alt-Backspace.

## Key gotcha (cost time)
- NOT DE-specific in the "muffin does something weird" sense, and NOT a build diff:
  lightdm session and manual TTY3 session run the **same** master.
- It reproduces on the **lightdm session (no trace)** but **not** under yserver
  `-trace` — full tracing slows the server and masks the race (**observer effect**;
  same class as the old fvwm "choppy"). Capture the app side instead: an **xtrace of
  just the client** (see `xfce-term.xtrace`) shows the protocol without perturbing
  server timing.

## Root cause (CONFIRMED via xfce-term.xtrace)
`XIGrabDevice` returns **status 0x01 (AlreadyGrabbed)** for both device 2 (pointer)
and device 3 (keyboard), issued right after a ButtonPress → the app's popup grab is
refused → dialog input-starved.

The AlreadyGrabbed decision lives in `crates/yserver-core/src/core_loop/process_request.rs`
~11651-11676. It reads grab **ownership** from one field and the **implicit-grab
exception flag** from a *different* field:

```rust
let held_by_other    = state.pointer_grab.is_some_and(|(owner, _)| owner != client_id);
let held_is_implicit = state.active_pointer_grab.is_some_and(|g| g.implicit);
u8::from(held_by_other && !held_is_implicit)
```

Those two structures **desync**. `crates/yserver-core/src/core_loop/pointer_fanout.rs:828-829`
(passive-grab activation) sets **only** `pointer_grab` + `pointer_grab_is_passive` and
leaves `active_pointer_grab` stale/None (`crates/yserver-core/src/server.rs:2690` same
pattern). Under Cinnamon a click activates a muffin passive/ancestor grab → `pointer_grab`
owned by muffin, `active_pointer_grab` None → `held_by_other && !held_is_implicit` = true
→ AlreadyGrabbed. The "don't block on an implicit grab" exception (added earlier for the
MATE combo hang) silently misses because it consults the desynced field.

## Architectural read (the real problem)
Grab **ownership** is spread across ~5 parallel fields, set in ~15 places, with fragile
cross-field invariants:
`pointer_grab`, `active_pointer_grab`, `pointer_grab_is_passive`, `active_keyboard_grab`,
`last_{pointer,keyboard}_grab_time` (see `server.rs` ~823-952).
Every DE-specific wedge (Steam click-swallow #94, MATE combo hang, now Cinnamon combo+
checkbox) got its own surgical exception keyed on a *subset* of those fields. Classic
"each fix reveals a new desync elsewhere → question the architecture."

## Fix direction
UNIFY grab ownership into a **single source of truth** — one grab record
`(owner, window, device, implicit, passive, mode, time)` set/cleared in one place per
transition — mirroring what `f6c45fd6` did for *freeze* state. Then AlreadyGrabbed (and
every other consumer) reads one consistent object and the desync class + the per-DE
exceptions collapse. Event routing/delivery can stay; only the STATE model is fragmented.
This is a bounded refactor, NOT the full input rewrite it feels like.

Recommended process (matches freeze-state unification): write a plan → codex review →
implement → regression-test against **Steam** (#94), **MATE combo**, **Cinnamon combo +
checkbox**, and **xts XI**; HW-verify each.

Stopgap option (discouraged — another patch on the pile): make `pointer_fanout.rs:828` and
`server.rs:2690` (and any other `pointer_grab`-only setters) also keep `active_pointer_grab`
in sync, OR derive implicit/passive-ness from the SAME field as ownership.

## Evidence / pointers
- `xfce-term.xtrace` (repo root) — the app-side trace with the AlreadyGrabbed replies.
- Memory: `project_issue96_chromium_maps_3d_glx_pbuffer.md` (GRAB BUG entry) has the full
  detail; related: freeze-state unification plan, `project_issue94_steam_xi_residuals`.
- Already shipped from the same #96 report (on master): `139e8a38` (Chromium GLX pbuffer /
  GPU accel), `8b62c939` (XI2 RawMotion relative-delta).
