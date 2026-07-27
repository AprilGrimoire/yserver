# Asynchronous implicit-sync Present source wait

Date: 2026-07-25
Branch: `fix/warframe-cursor-lag`

## Problem

`PresentPixmap` currently CPU-polls a DRI3 source dma-buf inline in the
single-threaded core loop. A long timeout freezes input; a short timeout races
the producer and can repeatedly copy stale pixels because Vulkan external
memory does not automatically participate in dma-buf implicit synchronization.
Warframe exposes both failures: the old 50 ms bound caused cursor lag, while
the 1 ms timing workaround can freeze the displayed game buffer in fullscreen
without a compositor.

## Scope

This phase fixes the implicit-sync `PresentPixmap` Copy path. It does not revive
Present Flip/DirectScanout or implement the separate Present 1.4 timeline
acquire wait.

## Design

1. Export the source dma-buf's READ-access sync-file without polling.
2. If it is already ready (or there is no producer fence), execute the existing
   copy path immediately.
3. Otherwise register the sync-file in the backend's stable deferred-PRESENT
   completion poller and return from request dispatch without copying.
4. Snapshot the request's update region and resolved source/destination
   identities in core state. The backend pins the exact source `DrawableId` so
   `FreePixmap` cannot destroy it while the producer fence is pending.
5. When the poller wakes, drain ready producer waits, execute the original
   Present copy, enqueue the existing GPU-completion/IdleNotify machinery, then
   release the source pin.

The producer wait and copy completion are deliberately distinct gates:

```
client render -> producer sync-file readable -> yserver copy submitted
              -> yserver copy completion -> IdleNotify/release fence
```

## Failure and teardown behavior

- No imported dma-buf or no attached producer fence: immediate copy.
- dma-buf sync-file export unsupported: warn once per request and retain the
  existing immediate-copy degradation; supported Linux dma-buf drivers use the
  asynchronous path.
- Poller registration failure: retain the fd and use the backend's 1 ms
  `next_wakeup` polling fallback. This polls readiness without blocking.
- A source freed while parked survives through the backend's exact
  `DrawableId` refcount pin.
- A destination destroyed before readiness makes the deferred copy a harmless
  no-op; the source pin is still released.

## Validation

- Unit tests for idle/ready/deferred/poll-fallback bookkeeping where practical.
- Existing Present wire and acceptance tests.
- `cargo +nightly fmt`.
- `cargo test --workspace` (or focused crates first if workspace runtime is
  prohibitive).
- `cargo clippy --all-targets -- -D warnings` exactly as CI.
- Hardware re-test on silence: Warframe windowed and fullscreen, Marco
  compositing both on and off. The hot path must show no inline op145 wait and
  the game buffer must continue updating.
