# Composite Overlay Window structural redesign

Status: Draft
Date: 2026-06-08
Branch: `feat/cow-structural`

## Purpose

Fix the Cinnamon/Mutter class of failures by correcting yserver's
Composite Overlay Window architecture instead of adding more
`cow_authoritative` exceptions.

The required change is structural:

- the COW must be a real window in the same tree the scene walker uses
- reparents to the COW must stay reparents, not collapse to root
- Manual-redirected windows must never emit directly to scanout

Once those rules hold, the compositor's stage naturally paints on top
because it lives under the topmost COW, exactly like Xorg.

## Problem statement

Today v2 mixes three models:

1. `resources.rs` contains a protocol-level COW resource
2. the backend allocates COW storage in the drawable store
3. the scene treats COW as a special extra draw, not a window node

That split model is the bug.

Concrete failures in current `master`:

- `GetOverlayWindow` allocates drawable storage, but does not create a
  real `windows_v2` node or `top_level_order` entry for the COW
  (`crates/yserver/src/kms/v2/backend.rs`)
- `reparent_subwindow` silently converts an unknown parent into
  `None`, which effectively means "top-level under root"
  (`crates/yserver/src/kms/v2/backend.rs`)
- `build_scene` appends a one-off COW draw after walking
  `top_level_order`, so the COW subtree is not represented by the real
  window recursion (`crates/yserver/src/kms/v2/scene.rs`)

When Mutter reparents its stage under the COW, yserver loses that
relationship. The stage becomes a top-level, sibling windows still emit
normally, and scanout order no longer matches the compositor model.

## Xorg model to follow

Xorg's model is simpler than yserver's current one:

- the COW is a normal `InputOutput` child of root
- it is never itself redirected
- it lives at the top of root's stacking order
- Manual-redirected windows are painted offscreen for the compositor,
  not directly to scanout

Relevant Xorg references:

- `composite/compoverlay.c`: COW create/destroy lifecycle
- `composite/compwindow.c`: reject redirecting the COW
- `composite/compwindow.c`: `CompositeRealChildHead` keeps the COW at
  the top when other root children are inserted/restacked

This design adopts that structure directly.

## Design goals

- one authoritative window tree
- no COW-specific scene mode flag
- no silent recovery from broken parentage
- Manual redirect semantics independent of compositor timing
- preserve existing Automatic redirect behavior

## Non-goals

- PictFormat tracking
- ARGB-vs-xRGB fidelity cleanup
- performance work to replace the old `cow_authoritative` shortcut
- protocol-visible `QueryTree` filtering to hide the COW if no client
  requires it yet
- introducing per-output COWs; this design keeps one global COW that
  spans the full root/screen space across all outputs, matching Xorg's
  single-screen compositor model

## Core invariants

1. `resources.rs` is the authoritative window hierarchy.
2. `windows_v2` is a backend projection of that hierarchy, not an
   independent model.
3. `top_level_order` is derived from root's children, excluding
   subwindows.
4. The COW exists either fully or not at all:
   - present in resources tree
   - present in backend window map
   - present in top-level stacking
   - backed by drawable storage
5. Manual-redirected windows do not emit to scanout.
6. A reparent to an unknown backend parent is a bug, not a fallback to
   root.
7. The COW is click-through: it may host compositor descendants, but it
   must not become the direct pointer target for root hit-testing.
8. The COW's parent is always `ROOT_WINDOW`.

## Proposed architecture

### 1. Materialize the COW as a real window node

On first `XComposite::GetOverlayWindow`:

- create the COW resource record if it does not already exist; do not
  pre-seed a live COW window at server init
- allocate backend storage for the fixed protocol xid
  `COMPOSITE_OVERLAY_WINDOW`
- populate the resources COW record's `host_xid`
- insert the COW into root's child list at the top
- create a matching `windows_v2` entry
- add it to `top_level_order` as the topmost top-level

On final `XComposite::ReleaseOverlayWindow`:

- remove the COW from `top_level_order`
- remove the backend window entry
- remove it from root's child list
- clear `host_xid`
- drop the drawable storage

There should be no state where only some of those are true.

Input semantics are part of materialization, not a follow-up. A
fullscreen mapped COW at the top of root would otherwise catch every
root hit-test. The materialized COW therefore must be click-through:

- its default input shape is empty, matching the Xorg/compositor model
- root hit-testing must never return the COW itself as the pointer
  target
- hit-testing must still descend into COW descendants, so compositor
  children like the stage remain reachable

In practice this means `direct_child_at` / `hit_test_children` must
treat the COW as a transparent container: descend through it when a
descendant matches, but never stop on the COW itself.

### 2. Remove the missing-parent fallback

`reparent_subwindow` currently does this:

- if parent xid is unknown in `windows_v2`, treat it as `None`
- `None` means "window is top-level under root"

That behavior masked the actual COW bug.

After the COW is materialized properly, reparents to it must resolve to
its real backend node. Any reparent to an unknown backend parent should
fail loudly.

Production behavior must be explicit:

- protocol-invalid reparents still fail at the request-validation layer
  with `BadWindow` / `BadMatch`
- backend projection mismatch after protocol validation is an internal
  consistency failure and must panic rather than log-and-continue

This is load-bearing. If the projection drifts from resources again,
continuing would silently corrupt scene structure.

### 3. Make the COW part of normal scene recursion

Delete the special "append one COW draw" path from `build_scene`.

The scene builder should only do this:

- walk `top_level_order`
- recurse through each real subtree

If the COW is the last top-level and the compositor stage is a child of
the COW, the stage appears on top without any special mode.

### 4. Make Manual redirect suppress direct emit unconditionally

Current scene behavior still depends too much on
`scene_participating` plus `redirected_target` plus
`under_redirected_ancestor`.

The rule needs to be explicit:

- Automatic redirect: emit, sourcing from the redirected backing
- Manual redirect: do not emit the window directly

Manual redirect means the compositor is now responsible for presenting
that content somewhere else, typically through a stage under the COW.

This rule must not depend on whether COW was recently presented to, or
whether a compositor has "registered" itself through a side channel.

### 5. Preserve alpha semantics by subtree, not by one-off draw

Today `alpha_passthrough=true` is attached to the special COW draw.

After the COW becomes a real scene node, alpha handling should follow
the subtree:

- draws outside the COW subtree use normal opaque behavior
- draws for the COW and its descendants inherit
  `alpha_passthrough=true`

The implementation mechanism should mirror the existing
`under_redirected_ancestor` recursion flag: `emit_window_subtree`
receives an inherited "under COW subtree" boolean, flips it on when the
walk enters the COW, and uses that flag when constructing draws.

This keeps the existing compositor blending intent while removing the
special-case scene path.

### 6. Keep the COW at the top by stack resolution, not by pinning

When root has a live COW child, other windows that move within root's
child list must land just below it.

That rule belongs in resource-level stacking resolution, because that is
the authoritative tree clients mutate. It should apply to:

- create-top-level
- restack to top
- restack above sibling when the sibling is the COW
- reparent back to `ROOT_WINDOW`
- circulate on `ROOT_WINDOW`

This should be implemented as one root-child insertion/reordering helper
rather than patching individual obvious call sites. Any operation that
mutates `root.children` must funnel through the same COW-aware policy.

Restacking the COW itself is still legal. The invariant is about where
other windows land, matching Xorg's behavior.

Separately, the COW itself is not reparentable:

- `ReparentWindow(COW, parent != ROOT_WINDOW)` returns `BadMatch`
- any request that would make the COW a descendant of itself returns
  `BadMatch`

### 7. Reject redirecting the COW

Once the COW is a real window in the normal model,
`RedirectWindow(COMPOSITE_OVERLAY_WINDOW, ...)` must return `BadMatch`.

That matches Xorg and prevents a nonsensical recursive redirect model.

## State ownership

After this change, ownership becomes:

- `resources.rs`
  - protocol-visible window tree
  - authoritative child ordering
  - COW presence under root
- `backend.rs`
  - faithful mirror in `windows_v2`
  - derived `top_level_order`
  - storage lifetime
- `scene.rs`
  - pure consumer of backend projections
  - no special COW lifecycle logic
  - no compositor-registration mode

## Expected code changes

### `crates/yserver-core/src/resources.rs`

- add one helper for all `root.children` insert/reorder operations so
  non-COW windows stay below a live COW
- use that helper in create, restack, reparent-to-root, and circulate
  paths
- stop creating a live COW window record during server init; instead
  create/materialize it on first `GetOverlayWindow` so the "exists
  fully or not at all" invariant is actually true
- reject reparenting the COW away from `ROOT_WINDOW`

### `crates/yserver-core/src/core_loop/process_request.rs`

- `GetOverlayWindow`: wire the protocol resource and invoke backend
  materialization
- `ReleaseOverlayWindow`: symmetric teardown on final release
- `RedirectWindow`: reject the COW with `BadMatch`
- `ReparentWindow`: keep protocol-invalid requests as `BadWindow` /
  `BadMatch`, but treat backend mirror failure after validation as a
  fatal internal error rather than dropping `Err`

### `crates/yserver/src/kms/v2/backend.rs`

- materialize/destroy a real `windows_v2` COW node
- stop using COW registration as a scene mode switch
- remove the missing-parent fallback from `reparent_subwindow`
- remove opportunistic COW arming based on recent presents
- make reparent projection failure fatal, not ignored

### `crates/yserver/src/kms/v2/scene.rs`

- delete the special COW append path
- recurse through the COW like any other top-level
- make Manual redirect suppression explicit
- propagate `alpha_passthrough` by COW subtree membership

### `crates/yserver-core/src/server.rs`

- update root hit-testing so the materialized COW is a transparent
  container rather than the direct pointer target

## What gets deleted

The following architectural idea should go away entirely:

- "COW exists in storage, then later becomes authoritative once enough
  compositor activity proves it should be"

In practice that means deleting the `register_cow` /
`unregister_cow` style control path and the associated
`cow_authoritative` scene behavior.

The same cleanup also deletes the support code that only exists to prop
up that model:

- `arm_cow_from_recent_present_if_needed`
- `present_to_cow_sources`
- `maybe_register_cow_on_paint`

The compositor does not "register a mode". It uses a normal window.

## Testing strategy

### Unit tests

- root stacking preserves COW as topmost child
- top-level create/restack lands below COW
- reparent to root lands below COW
- circulate on root preserves COW as topmost child
- restacking the COW itself still works
- `GetOverlayWindow` materializes a backend node and top-level entry
- final `ReleaseOverlayWindow` removes both
- reparent to COW sets a real backend parent and removes the child from
  `top_level_order`
- reparent to an unknown parent fails loudly
- reparenting the COW away from root returns `BadMatch`
- Manual-redirected windows produce no scene draw
- Automatic-redirected windows still produce a draw
- COW subtree draws inherit `alpha_passthrough=true`
- `RedirectWindow(COW)` returns `BadMatch`
- root hit-test does not return the COW itself, but still reaches a COW
  descendant

### Integration tests

- compositor stage reparents under the COW and is emitted exactly once
- Manual-redirected siblings no longer appear as direct scanout draws
- stage ordering is structurally correct without a special COW append
- compositor-session pointer events still reach real stage/dialog
  descendants rather than stopping on the fullscreen COW

### Hardware validation

- Cinnamon/Mutter:
  - gnome-keyring unlock prompt appears above the parent window
  - seahorse auth prompt appears above the parent window
  - cinnamon-session logout dialog appears above the dimmed desktop
- Marco/Xfwm4:
  - no regressions in compositor-driven desktops
- non-composited session:
  - unchanged behavior

## Risks

- removing `cow_authoritative` may expose performance regressions in
  compositor sessions
- stack-order logic around root/COW may affect older tray/applet fixes
- resource/backend mirror bugs will become hard failures rather than
  visual corruption

Those are acceptable tradeoffs. The current behavior is structurally
wrong and has already consumed multiple rounds of brittle fixes.

## Why this is better than another surgical fix

The current failure did not happen because one condition was wrong in
scene assembly. It happened because yserver split one X11 object across
three unrelated models and then papered over the gaps with mode flags.

Any design that keeps these properties is still unstable:

- COW is not a real backend window
- unknown reparents collapse to root
- Manual redirect visibility depends on COW registration timing
- scene assembly has a COW-only draw path

This proposal removes all four.

## Done criteria

This work is done when:

- the COW is a real root child in both resources and backend state
- reparents to the COW survive end-to-end
- the scene has no special COW append path
- Manual redirect no longer emits directly to scanout
- Cinnamon's prompt-under-parent regressions are fixed on hardware
- Marco/Xfwm4/non-composited sessions do not regress
