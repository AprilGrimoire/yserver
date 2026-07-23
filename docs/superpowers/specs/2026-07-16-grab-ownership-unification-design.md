# Grab-Ownership Unification + Xorg-Faithful Grab Semantics — Design Spec

Date: 2026-07-16
Status: proposed (revised after codex review + grab-lifetime audit)
Branch: `fix/grab-ownership-unify`
Related: `docs/handoff-xi2-grab-ownership-desync.md`, freeze-state unification (`f6c45fd6`),
memory `project_xi_grab_delivery_redesign`.

## Problem

The active pointer grab is spread across three parallel fields that must move in lockstep but
don't: `pointer_grab: Option<(ClientId, ResourceId)>`, `pointer_grab_is_passive: bool`,
`active_pointer_grab: Option<ActivePointerGrab>`. Passive-grab activation
(`pointer_fanout.rs:829`) sets the first two but leaves `active_pointer_grab` **None**; nothing
downstream can tell "this active `pointer_grab` is a live passive grab" from a stale one by reading
`active_pointer_grab`.

This has produced **four** grab bugs, each previously patched with a surgical exception on a *subset*
of the fields — a non-Xorg "supersede" rule that codex review flagged. The reframe below fixes the
model, not the guard.

### Confirmed via fail/pass traces (yserver `xfce-term.xtrace` vs Xorg `xfce-term-xorg.xtrace`)
The app's behavior is byte-identical in both — a `ButtonPress` is delivered to the app, then the app
issues `XIGrabDevice` (device 2 + 3) *while the button is still down*:
- **Xorg**: both grabs reply **Success** (status `0x00`).
- **yserver**: both reply **AlreadyGrabbed** (`0x01`) → GTK dialog input starved.

So the difference is **purely server-side grab state**: at grab-time yserver has a *foreign* grab
active that Xorg does not.

### The three intertwined root defects (reconciled: codex + grab-lifetime audit + the traces)
Xorg's `GrabDevice` (`dix/events.c:5240`) returns `AlreadyGrabbed` for **any** foreign active grab —
**no** passive/implicit exemption. So the app's `Success` under Xorg means no *foreign* grab is
active there. yserver diverges in three ways:

1. **State desync.** Passive activation never populates `active_pointer_grab`; the `AlreadyGrabbed`
   guard reads ownership from `pointer_grab` but the (mis-added) implicit/passive exemption from
   `active_pointer_grab` — inconsistent snapshot. (`process_request.rs:11671-11675`,
   `pointer_fanout.rs:826-830`.)
2. **Implicit grab mis-attributed.** yserver captures the implicit grab core-first and can attribute
   it to a foreign core ancestor rather than the press recipient. Under Xorg the implicit grab here
   is the app's own → the app's `XIGrabDevice` is `SameClient` → replaces → Success. (This is the
   MATE-combo case the shipped `3312e24e` "supersede" hack papered over.)
3. **Passive-grab lifetime.** yserver releases a passive grab only on the *ButtonRelease*
   (`release_passive_grab_on_button_release`, `pointer_fanout.rs:2596`, fires on any release while
   passive) — but in the repro the app grabs *before* the release (button held). Under Xorg muffin's
   passive grab is already released by the WM's `AllowEvents`/replay (Xorg deactivates on
   `NOT_GRABBED`/Replay and on final release, `Xi/exevents.c:1935`, `dix/events.c:1898`). yserver
   holds it past that point → foreign grab active → `AlreadyGrabbed`. (This is the Cinnamon case.)

The "supersede" rule masked #2/#3 by letting an explicit grab bulldoze the lingering foreign grab —
non-Xorg, and it will keep generating this bug class until the model is fixed.

## Goal

One source of truth for the active pointer grab, with **Xorg-faithful** guard, implicit-grab
attribution, and grab lifetime. Every consumer reads one consistent record; the desync class and the
per-DE exception pile collapse. **Event delivery/routing behavior is a goal to preserve, not a
free assumption** — its readers are migrated and revalidated (codex).

## Design

### 1. Single record (foundation)
`active_pointer_grab: Option<ActivePointerGrab>` becomes the **sole** pointer-grab-ownership state.
`ActivePointerGrab` gains `passive: bool`. **Passive activation snapshots the matched
`PassiveButtonGrab`'s delivery-relevant fields** (`owner_events`, `via_xi2`, `event_mask`,
**`xi2_mask`**, cursor, grab_window) into the record — not just `passive: true` — so delivery no
longer re-scans `button_grabs` by the ambiguous `(owner, grab_window)` key (codex #3). `xi2_mask` is
required or grabbed XI2 delivery regresses (`pointer_fanout.rs:1295`). **Delete `pointer_grab` and
`pointer_grab_is_passive`.** `last_pointer_grab_time` stays (persists past grab end).

### 2. Writes through one mutation path
Every write (~67 sites) → `set_pointer_grab(record)` / `clear_pointer_grab()`. Passive activation and
implicit install both build the full record — desync is impossible (one field).

### 3. Reads go direct to the record (~130 sites)
No `pointer_grab`/`pointer_grab_is_passive` accessors (full removal).

### 4. AlreadyGrabbed guard — pure Xorg, both devices
Only rule: a grab held by **another client** (any kind) → `AlreadyGrabbed`; `SameClient` → replace
(`dix/events.c:5240`). **Drop the implicit/passive supersede exemption entirely.** Correctness now
comes from #5 and #6 (attribution + lifetime), not from relaxing the guard.

### 5. Implicit grab owner selection = Xorg's `ActivateImplicitGrab`, exactly
Reproduce Xorg's implicit-owner selection **precisely** — not a generic "press recipient." Xorg picks
the specific `(client, pWin, deliveryMask)` chosen by `DeliverEventsToWindow` before
`ActivateImplicitGrab`: **core events delivered first**, `DeviceButtonGrabMask` favored among
same-window subscribers (`dix/events.c:2150,2268,2362,2415`). A foreign **core ancestor can
legitimately own** the implicit grab in Xorg too — so we do **not** force the app to own it.

The pure-Xorg guard (#4) is safe **only because** yserver then matches Xorg's owner exactly: whatever
Xorg returns (Success via `SameClient`, or `AlreadyGrabbed`), yserver returns the same. The MATE combo
works on Xorg, so faithful reproduction makes it work on yserver **by construction** — mirroring
Xorg's selection, not engineering a SameClient outcome. yserver's current core-first capture is an
*approximation* that diverges (the MATE bug + the `3312e24e` supersede hack); the task is to match
Xorg's selection rule.

**This is the single biggest risk / correctness crux**: reproducing Xorg's implicit-owner selection
across mixed core/XI2 delivery is what makes the pure guard correct. It must be pinned by tests
against Xorg-observed outcomes, not asserted.

### 6. Passive/implicit grab lifetime = Xorg's DeactivateGrab triggers
Release exactly when Xorg does:
- final `ButtonRelease` with `buttons_down == 0` for a from-passive/implicit grab
  (`Xi/exevents.c:1935`), and
- the WM's `AllowEvents`/`XIAllowEvents` `ReplayPointer`/`ReplayDevice` (`NOT_GRABBED`,
  `dix/events.c:1898`), and the other shared `DeactivatePointerGrab` sites.

The **exact** release path that leaves muffin's passive grab lingering in the Cinnamon repro will be
pinned by TDD during implementation (bee cannot reproduce the HW race; the deterministic tests are
the gate). Align yserver's release triggers to Xorg's; remove the non-Xorg
release-on-any-ButtonRelease behavior if it diverges.

### 7. Out of scope (unchanged)
`button_grabs`/`key_grabs` (passive *registrations*), `xi1_active_grabs` (XI1 device grabs). Keyboard
(`active_keyboard_grab`) is already single-source; only its guard rule aligns to #4.

## Migration (codex #2, #4 — stricter than "just delete")
1. Add `passive` + the snapshot to `ActivePointerGrab`; add `set/clear_pointer_grab`.
2. **Dual-write phase**: passive activation (and every writer) populates the full record *while the
   legacy fields still exist*, with debug assertions that the two representations agree.
3. Migrate all delivery/routing **readers** (XI2 passive-grab gating `pointer_fanout.rs:1224`,
   `active_grab_target` `:2423`, core Step-2 redirect, passive-release teardown `:2597`, replay
   `process_request.rs:20145`) onto the record and **revalidate** — do not assume routing is
   unchanged.
4. Land the guard (#4), attribution (#5), and lifetime (#6) **together as one series**, with their
   tests green **before** the `3312e24e` supersede exemption is removed. **Ordering hazard (codex
   #3/D):** dropping the supersede exemption while attribution/lifetime are still wrong would
   immediately re-break the MATE combo and the Cinnamon dialog. So the exemption is the **last**
   thing removed, gated on the new tests passing — there is no window where the pure guard runs
   without correct attribution+lifetime.
5. Delete `pointer_grab` + `pointer_grab_is_passive` only after every reader/writer is migrated and
   the dual-write assertions have held across the suite.

## Testing / acceptance
Deterministic unit tests are the **primary** gate (HW race is flaky; bee can't repro):
1. **Reframe the committed RED pin** `xi_dialog_grab_succeeds_over_foreign_passive_grab_cinnamon`
   (`afa955ab`): it currently asserts the *supersede* behavior (grab succeeds over a *held* foreign
   passive grab), which is **not** the Xorg-faithful invariant. Rewrite it to model the Xorg
   *sequence* — the WM's `AllowEvents`/replay **deactivates** its passive grab, so the app's later
   grab succeeds because **no foreign grab remains** (guard stays pure-Xorg). Acceptance gate.
2. Steam #94 clobber — explicit-vs-explicit still `AlreadyGrabbed`.
3. MATE combo — succeeds because the implicit grab is the app's own (`SameClient`), not via supersede;
   then **remove the `3312e24e` supersede hack** and confirm the suite stays green.
4. Per-transition state-consistency assertions (one field ⇒ no desync can reappear).
5. Existing grab suite + xts XI; HW smoke on the WM matrix where a repro exists.

## Risks & mitigations
- **Scope grew** from a one-field unification to unify + faithful guard/attribution/lifetime, in a
  delicate WM-tuned area. Mitigation: unify **first** (foundation), then land guard/attribution/
  lifetime corrections on the single model; dual-write+assert phase; the regression suite is the
  guard; `cargo test` green at each step.
- **Exact release path unpinned** (no reproducible HW). Mitigation: TDD surfaces it — the reframed
  RED test drives the real sequence; align to Xorg's DeactivateGrab triggers.
- **Shipped code to reconcile**: `b45ca134`'s `AlreadyGrabbed` is the pure-Xorg guard and **stays**
  (it is exactly #4). `3312e24e`'s implicit/passive **supersede exemption** is the non-Xorg hack this
  work **removes** (replaced by correct attribution #5 + lifetime #6). `440ad099` (core-implicit XI2
  delivery) is a delivery fix, Xorg-faithful, and stays. All work on HW today. The supersede
  exemption is removed **last**, gated on the new attribution+lifetime tests (migration step 4), so
  there is no window where the pure guard runs without correct attribution/lifetime.

## Resolved question
Supersede vs release: **release/attribution**, confirmed. Xorg (`dix/events.c:5240`) blocks on any
foreign active grab; the app's `Success` comes from the foreign grab being *gone* (released) or the
grab being the app's *own* (SameClient) — not from Xorg permitting explicit-over-foreign.
