# superpowers doc retention audit (2026-07-11)

## Summary

Pre-cleanup inventory of `docs/superpowers` on 2026-07-11:

- 102 plan files
- 53 spec files
- 19 findings files
- 6 notes files
- 1 status handoff file

Reference classification against the **live** tree (`README.md`,
`docs/status.md`, `docs/known-issues.md`, `crates/`, `Justfile`):

- **Keep in place:** 36 files
- **Archive-only:** 17 files
  These are only referenced from `docs/status-archive-*`.
- **Currently unreferenced:** 131 files

This makes the current shape clear: most of `docs/superpowers` is
migration archaeology. The active project no longer needs the majority
of this tree in its current location.

## Keep

These are still referenced from live docs, code comments, or the
Justfile and should stay where they are until those references change.

### findings

- `docs/superpowers/findings/2026-05-31-render-source-picture-redirect.md`
- `docs/superpowers/findings/2026-06-09-glx-tfp-radv-export-rootcause.md`
- `docs/superpowers/findings/2026-06-18-pointer-stacking-dual-authority-diagnosis.md`
- `docs/superpowers/findings/2026-06-25-altgr-4level-golden-vector.md`
- `docs/superpowers/findings/2026-06-25-xkb-indicator-compat-golden-vector.md`
- `docs/superpowers/findings/2026-07-08-mate-compositor-drag-smear-diagnosis.md`
- `docs/superpowers/findings/2026-07-08-perf-thread-wm-redirect-model.md`
- `docs/superpowers/findings/2026-07-08-xorg-render-optimization-gaps.md`

### plans

- `docs/superpowers/plans/2026-05-06-single-threaded-core.md`
- `docs/superpowers/plans/2026-05-16-stage-2.md`
- `docs/superpowers/plans/2026-05-16-stage-3.md`
- `docs/superpowers/plans/2026-05-17-stage-4.md`
- `docs/superpowers/plans/2026-05-20-cow-authoritative-mode.md`
- `docs/superpowers/plans/2026-05-20-stage-5-make-v2-fast.md`
- `docs/superpowers/plans/2026-05-21-descriptor-pool-ring.md`
- `docs/superpowers/plans/2026-05-23-deferred-present-completion.md`
- `docs/superpowers/plans/2026-05-23-frame-builder-submit-rate-phase-a.md`
- `docs/superpowers/plans/2026-05-24-frame-builder-phase-b1.md`
- `docs/superpowers/plans/2026-05-24-frame-builder-phase-b2.md`
- `docs/superpowers/plans/2026-05-25-frame-builder-phase-b3.md`
- `docs/superpowers/plans/2026-05-28-vt-switching.md`

### specs

- `docs/superpowers/specs/2026-05-05-single-threaded-core-design.md`
- `docs/superpowers/specs/2026-05-07-phase4-1-vulkan-compositor-design.md`
- `docs/superpowers/specs/2026-05-09-phase4-2-dri3-present-glx-design.md`
- `docs/superpowers/specs/2026-05-15-rendering-model-v2.md`
- `docs/superpowers/specs/2026-05-21-descriptor-pool-ring-design.md`
- `docs/superpowers/specs/2026-05-23-deferred-present-completion-design.md`
- `docs/superpowers/specs/2026-05-24-frame-builder-phase-b-design.md`
- `docs/superpowers/specs/2026-05-25-frame-builder-phase-b3-design.md`
- `docs/superpowers/specs/2026-05-27-vt-switching-design.md`
- `docs/superpowers/specs/2026-06-08-cow-structural-design.md`
- `docs/superpowers/specs/2026-06-10-animated-cursors-design.md`
- `docs/superpowers/specs/2026-06-12-lightdm-launch-design.md`
- `docs/superpowers/specs/2026-06-12-xcmisc-design.md`
- `docs/superpowers/specs/2026-06-14-idle-compositor-cursor-damage.md`
- `docs/superpowers/specs/2026-06-25-xauth-server-auth-design.md`

## Archive

These should not stay mixed into the live tree, but they are still
worth keeping if you want local archaeology beyond git history.

### Archive-only

These are referenced only from `docs/status-archive-*` and can move
under an archive subtree together with those archive status docs.

- `docs/superpowers/notes/2026-05-04-phase6-5-fvwm3-trace.md`
- `docs/superpowers/plans/2026-05-02-phase3-6-plan.md`
- `docs/superpowers/plans/2026-05-12-rendering-rearchitecture-phase3.md`
- `docs/superpowers/plans/2026-05-13-kms-teardown-fix-results.md`
- `docs/superpowers/plans/2026-05-13-kms-teardown-fix.md`
- `docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase3b-results.md`
- `docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase3c-results.md`
- `docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase3d-results.md`
- `docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase3e-results.md`
- `docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase3f-1-results.md`
- `docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase3f-2-results.md`
- `docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase4-results.md`
- `docs/superpowers/plans/2026-05-14-gpu-trap-rasterization-results.md`
- `docs/superpowers/plans/2026-05-14-pixmap-allocation-pool-results.md`
- `docs/superpowers/plans/2026-05-14-rendering-rearchitecture-phase5-results.md`
- `docs/superpowers/specs/2026-05-01-phase3-5-extension-completion-design.md`
- `docs/superpowers/specs/2026-05-02-phase3-6-design.md`

### Unreferenced but plausible archive material

131 files are not referenced from the live tree or archive status docs.
Most are old plans/specs from the migration and feature-phase period.

Recommended treatment:

1. Move the whole set under a dedicated archive root if you still want
   local grep access.
2. Do not keep them mixed with live docs; they create false authority.

Good archive buckets:

- old phase plans/specs from 2026-04 through early 2026-06
- superseded implementation plans whose features already shipped
- one-off notes and handoff material
- platform feasibility findings that are not driving current work

## Delete-first candidates

Deleted in this cleanup:

- `docs/superpowers/findings/2026-06-18-xorg-xts-baseline.tsv`
- `docs/superpowers/findings/2026-06-18-xorg-xts-journal.gz`
- `docs/superpowers/plans/dead-ends/2026-05-12-paint-composite-sync-rework.md`
- `docs/superpowers/specs/dead-ends/2026-05-12-paint-composite-sync-design.md`
- `docs/superpowers/specs/dead-ends/POSTMORTEM.md`
- `docs/superpowers/notes/2026-05-03-phase6-2-host-surface-audit.md`
- `docs/superpowers/notes/2026-05-07-phase6-10-validation.md`
- `docs/superpowers/notes/2026-05-07-phase6-10-vng-recipe.md`
- `docs/superpowers/notes/2026-05-09-sync-audit.md`
- `docs/superpowers/notes/2026-05-18-stage-4d-compose-debug-checkpoint.md`
- `docs/superpowers/status/2026-06-17-randr-multimonitor-handoff.md`

Rationale:

- generated artifacts (`.tsv`, `.gz`) are the least useful in-tree
- dead-end subtrees are explicitly historical
- notes/handoffs are high-churn working material, not canonical docs

One-line retention for the deleted dead-end subtree:

- An early paint/composite sync rework was attempted, rejected, and is
  now intentionally preserved only as this summary plus git history.

## Recommended execution order

1. Keep the 36 live files in place.
2. Move archive-only files together with `docs/status-archive-*` under a
   dedicated archive subtree.
3. Delete the low-risk generated artifacts / dead-ends / notes first.
4. Move the remaining 120+ unreferenced plans/specs/findings into the
   same archive subtree.
5. After that, shrink `docs/status.md` so it links only to the retained
   canonical docs.

## Reference hygiene

During this audit, these live-doc reference issues were found:

- `docs/status.md` referenced missing plan names for the old
  cursor-bundling experiment and the parked render-source-picture plan.
- `docs/status.md` used a brace-expanded pseudo-path for the GLX TFP
  spec/plan link.
- `docs/known-issues.md` used a wildcard path for the 2026-06-25 XKB
  findings.

Those were fixed in the same change so the current live docs no longer
point at obviously missing `docs/superpowers` paths.
