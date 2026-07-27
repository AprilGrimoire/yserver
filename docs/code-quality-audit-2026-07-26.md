# Code quality audit — 2026-07-26

This document captures a repository-wide code-quality review so the findings
do not need to be re-derived. The review covered technical-debt markers,
module and crate boundaries, protocol stubs, runtime invariant handling,
test structure, and current project documentation. It did not change runtime
code.

## Baseline

Reviewed on `master` at `8cf45085` with a clean working tree.

Validation was green:

- `cargo +nightly fmt --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo test --workspace`

The workspace test run reported 1,967 passing tests and 173 ignored tests.
The ignored set is primarily hardware, live-Vulkan, and acceptance coverage.

The repository is healthy at the formatting, lint, and default-test level.
The findings below are maintainability risks, explicit implementation gaps,
and latent correctness risks rather than current CI failures.

## Prioritized findings

### 1. KMS output assignment is incomplete for shared hardware resources

Priority: high.

Ownership: an external contributor is working on this item as of 2026-07-26.
Coordinate before changing the assignment implementation to avoid duplicate
work.

`crates/yserver/src/drm/modeset.rs::assign_outputs` uses greedy first-fit
assignment for connector, CRTC, and primary-plane combinations. The TODOs at
the function and discovery path explicitly note that Intel and AMD hardware
can expose shared encoder, CRTC, and plane pools that require matching rather
than greedy selection.

Consequences:

- A valid multi-output topology can fail depending on connector enumeration
  order.
- The current comments describe virtio-gpu as the safe scope even though real
  Intel and AMD systems are primary project targets.

Recommended direction:

- Build the connector-to-CRTC/plane compatibility graph.
- Use a deterministic maximum bipartite matching algorithm.
- Add fixtures where greedy selection strands a connector but a valid global
  assignment exists.
- Preserve deterministic output ordering when more than one solution exists.

Relevant code: `crates/yserver/src/drm/modeset.rs:193-260`.

### 2. Unsupported protocol operations can report false success

Priority: high.

Ownership: selected as the first internal code-quality phase on 2026-07-26.
The implementation plan is
`docs/superpowers/plans/2026-07-26-protocol-silent-success-audit.md`.

At the audit baseline, the central request dispatcher logged unknown opcodes
and returned `RequestOutcome::Handled`. It also had explicit no-op success
paths for `GrabServer`, `UngrabServer`, and `RecolorCursor`. Several extension
handlers contain deliberate empty or partial success paths:

- RANDR `RRCreateMode`, `RRSetPanning`, and `RRCreateLease` return a stopgap
  `BadImplementation` instead of implementing Xorg behavior.
- Some X-Resource queries return zero because there is no byte or PID
  accounting.
- GLX texture-from-pixmap binding can succeed even though indirect texture
  sampling does not update the texture contents.
- Several valid XI1 requests return empty replies or perform no state change
  after validation.

Consequences:

- Clients cannot reliably distinguish implemented behavior from a silent
  server omission.
- Failures surface later and farther from the responsible request.
- Advertised extension versions and capabilities can overstate useful
  implementation coverage.

Recommended direction:

- Inventory every log-only, zero-reply, and no-op success path.
- Compare each path with Xorg and the applicable protocol specification.
- Implement the operation, advertise a lower capability/version, or return
  the Xorg-compatible protocol error.
- Add regressions asserting both the wire result and absence of unintended
  state changes.

Progress on `quality/protocol-stub-audit`:

- Unknown core opcodes and out-of-table extension minors now return Xorg's
  `BadRequest` wire error; core `NoOperation` and GLX's extension-specific
  error remain intentional exceptions.
- `GrabServer` / `UngrabServer` now implement cross-client scheduling and
  release the grab when its owner disconnects.
- `GetMotionEvents` validates the window before returning yserver's
  intentional empty-history reply.
- `RecolorCursor` rejects unknown cursors with `BadCursor`; valid monochrome
  cursors retain pixel roles and are recolored in KMS or forwarded by ynest,
  while ARGB cursors preserve Xorg's intentional no-op behavior.
- Known extension operations with partial behavior remain Phase 4 work in the
  linked implementation plan. RANDR provider requests now return
  `BadProvider` instead of hanging or silently succeeding while the server
  advertises no providers; `FreeLease` similarly returns `BadLease` while no
  lease can be created. RANDR resource queries now validate windows, outputs,
  and CRTCs before replying, and `SetOutputPrimary` updates tracked state
  instead of silently succeeding. X-Resource now computes live padded pixmap
  storage and reports ClientXID identities; peer PID retention and recursive
  resource-size accounting remain open. XI1 `GetSelectedExtensionEvents` now
  reports real per-window this-client and all-client selections; the selection
  store is canonical per window, with server-wide delivery state derived from
  it. Window teardown now clears core, XI1, and XI2 subscriptions together
  instead of leaving extension masks stale. The obsolete blanket XI1
  zero-stub marker was removed. `DeviceBell` now returns Xorg's `BadValue`
  for yserver's bell-less feedbacks rather than silently succeeding.
  `ChangeFeedbackControl` now updates the shared keyboard/pointer controls,
  including big-endian requests. Xorg comparison confirmed that relative axes
  without physical metadata correctly use a `0/0/0` resolution range; its
  zero round trip and big-endian request are now covered. A single bounded
  motion-history store at the translated pointer fanout now serves both core
  and XI1 queries, completing this XI1 audit group without duplicating input
  ownership. XTEST `CompareCursor` now compares inherited window cursor state
  (including `None` and `XTestCurrentCursor`) and validates both resources
  instead of always claiming a match.

Relevant code:

- `crates/yserver-core/src/core_loop/process_request.rs:79-83`
- `crates/yserver-core/src/core_loop/process_request.rs:203-225`
- `crates/yserver-core/src/core_loop/process_request.rs:411-420`
- `crates/yserver-core/src/core_loop/process_request.rs:3205-3216`
- `crates/yserver-core/src/core_loop/process_request.rs:9607-9613`
- `crates/yserver-core/src/core_loop/process_request.rs:10244-10249`
- `crates/yserver-core/src/core_loop/process_request.rs:12869-12879`

### 3. Core dispatch and KMS rendering have grown into god modules

Priority: high.

Largest source files at the audit baseline:

| File | Total lines | Production boundary |
| --- | ---: | ---: |
| `core_loop/process_request.rs` | 49,310 | tests start at 25,452 |
| `kms/render/backend.rs` | 28,772 | tests start at 18,548 |
| `kms/render/engine.rs` | 15,569 | tests start at 11,643 |
| `tests/render_acceptance.rs` | 8,231 | integration tests |
| `core_loop/pointer_fanout.rs` | 6,522 | tests start at 3,195 |
| `kms/render/scene.rs` | 5,618 | tests start at 3,417 |

The `Backend` trait is about 2,261 lines and exposes roughly 193 methods. It
combines server lifecycle, input, RANDR, rendering, cursor, XKB, DRI3,
Present, VT, and host-X11 responsibilities.

Consequences:

- Changes have a large compile and review surface.
- Protocol, policy, and platform implementation are difficult to test in
  isolation.
- The large interface makes test doubles expensive and encourages defaults
  that silently do nothing.
- Ownership rules are described in comments rather than enforced by module
  boundaries and types.

Recommended decomposition:

1. Split `process_request.rs` by core protocol area and extension while
   retaining one small top-level opcode router.
2. Introduce narrow backend ports such as display/RANDR, rendering, input,
   Present, DRI3, and XKB interfaces. Do this incrementally; avoid a single
   flag-day trait replacement.
3. Move RANDR registry, cursor management, clipping, font rendering, Present
   completion, and input routing out of `KmsBackend` into owned components.
4. Move large in-file test modules alongside the extracted production
   modules so tests follow the same subsystem boundaries.

Relevant code:

- `crates/yserver-core/src/core_loop/process_request.rs`
- `crates/yserver-core/src/backend/trait_def.rs`
- `crates/yserver/src/kms/render/backend.rs`
- `crates/yserver/src/kms/render/engine.rs`

### 4. Render-state invariants are frequently enforced by panicking

Priority: medium.

The production portion of `kms/render/engine.rs` contains approximately 180
`unwrap`, `expect`, or explicit panic-style invariant checks. Many repeatedly
unwrap `self.inner`, the currently open frame, initialized render assets, or
previously validated store entries. `KmsBackend::reparent_subwindow` also
deliberately panics when the resource tree and backend window projection
diverge.

Most sites represent legitimate internal invariants, not unchecked client
input. The density is nevertheless a design signal: lifecycle states are
represented as `Option` and enforced repeatedly at runtime even though many
surrounding APIs return `io::Result`.

Consequences:

- A single lifecycle or projection bug terminates the display server.
- The valid calling sequence is difficult to infer and preserve during
  refactoring.
- Repeated checks obscure the actual render operation.

Recommended direction:

- Separate initialized and uninitialized engine states at the type/API level.
- Use an open-frame guard or context that exposes only operations valid while
  a frame is open.
- Replace repeated `expect("inner")`/`expect("open")` calls with one checked
  boundary per operation.
- For cross-layer projection drift, prefer a detailed fatal error routed
  through orderly server shutdown, or a proven resynchronization path, over
  an unconditional process panic.

Relevant code:

- `crates/yserver/src/kms/render/engine.rs:2322`
- `crates/yserver/src/kms/render/engine.rs:3800-3837`
- `crates/yserver/src/kms/render/backend.rs:12291-12300`

### 5. The shared backend test double is incomplete and tightly coupled

Priority: medium.

`RecordingBackend` intentionally implements unexercised `Backend` methods
with `unimplemented!()`. Adding handler coverage therefore often requires
extending a large global mock before the test can run.

Consequences:

- Cross-layer tests can panic for test-infrastructure reasons rather than
  report an assertion failure.
- The test double mirrors the oversized production trait.
- It discourages focused handler tests for less-used drawing paths.

Recommended direction:

- Let narrow backend traits produce small, subsystem-specific recorders.
- Where a method is not material to a test, use explicit safe defaults.
- Keep deliberate failure behavior only for calls the test declares must
  never occur.

Relevant code:

- `crates/yserver-core/src/backend/recording.rs:1-18`
- `crates/yserver-core/src/backend/recording.rs:671-677`
- `crates/yserver-core/src/backend/recording.rs:839-1053`

### 6. Current-status and source documentation contain stale migration state

Priority: medium.

At the audit baseline:

- `docs/status.md` names `fix/idle-compositor` as the active branch while the
  checkout is on `master`.
- It says `kms/scheduler/` survives, but that directory no longer exists.
- It describes Stage 5 as the next work even though later work is documented
  above that section.
- `kms/render/backend.rs` still introduces the production backend as a Stage
  1b skeleton whose rendering methods are stubs.
- Many render module comments describe historical v1/v2 migration phases
  rather than current ownership and invariants.

Consequences:

- `docs/status.md` cannot be treated as fully authoritative without checking
  the tree and history.
- Historical implementation detail makes present-day module behavior harder
  to understand.

Recommended direction:

- Refresh the active branch and next-work sections whenever a phase closes.
- Remove claims about deleted directories.
- Rewrite module-level documentation in present tense around current
  responsibilities and invariants.
- Retain migration narratives in the existing archive, spec, and plan
  documents rather than production module headers.

Relevant code and docs:

- `docs/status.md:88`
- `docs/status.md:103`
- `docs/status.md:245`
- `crates/yserver/src/kms/render/backend.rs:1-18`

### 7. Smaller operational debt remains embedded in hot paths

Priority: lower, unless a related runtime problem is being investigated.

- Atomic-commit retry uses a hardcoded 100 ms delay. The comment recommends
  making it tunable and observable through telemetry.
- Direct-mode input enumeration still has a startup ordering race between
  device discovery and early client queries.
- The nested backend does not implement the background repaint behavior used
  for missing-source `CopyArea`/`CopyPlane` regions.

Relevant code:

- `crates/yserver/src/kms/render/scene.rs:1923-1931`
- `crates/yserver-core/src/core_loop/run.rs:419-428`
- `crates/yserver-core/src/backend/trait_def.rs:1162-1180`

## Existing TODO inventory found during the audit

This is not an exhaustive feature backlog; it records explicit source TODOs
that affect correctness, observability, or maintainability:

- Proper shared-resource KMS output matching.
- RANDR CreateMode, SetPanning, and CreateLease.
- X-Resource byte and PID accounting.
- Indirect GLX texture sampling after texture-from-pixmap binding.
- Real XI1 success behavior for currently validated zero-stubs.
- Active cursor name plumbing for cursor-image-and-name replies.
- Depth-1 bitmap mask rasterization for a remaining clip path.
- Direct-mode startup input enumeration ordering.
- Tunable and observable KMS commit-retry backoff.
- Nested backend background repaint forwarding.
- Several acceptance-test scaffolding and live-Vulkan coverage gaps.

Protocol TODOs should also be reflected in `docs/known-issues.md` when they
are externally observable. Implementation-structure work can remain in this
audit until it is promoted into a phased implementation plan.

## Suggested cleanup sequence

1. Replace greedy KMS output assignment with deterministic matching.
2. Audit protocol no-op/success behavior and align errors/capabilities with
   Xorg.
3. Split extension handlers out of `process_request.rs` without changing
   behavior.
4. Extract narrow backend interfaces and subsystem-specific test doubles.
5. Introduce render-engine lifecycle types/guards, then reduce invariant
   panics at subsystem boundaries.
6. Refresh source-level module documentation as each extraction lands.
7. Address the smaller operational TODOs alongside related runtime work.

Each behavioral phase should continue to use Xorg as the compatibility oracle
where it differs from a literal reading of the specification, and should run
nightly formatting, CI-equivalent Clippy, and the relevant unit, acceptance,
and hardware tests before merge.
