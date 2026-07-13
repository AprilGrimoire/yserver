# Reverse-PRIME copy implementation plan

1. Separate renderer submission from transport/presentation in the scene path
   without changing existing shared-buffer behavior.
2. Introduce the `OutputScanout` ADT and one per-output in-flight-frame ADT.
3. Generalize the native completion poller enough to return stable ready job
   tokens, add the scanout-render-completion backend fd kind, and cover core
   dispatch with a recording backend.
4. Add a minimal per-sink Vulkan transfer context and dedicated A source/B
   destination allocation helpers with exact usage flags.
5. Implement full-depth copy probing and append it after the two copy-free
   candidates.
6. Implement the runtime A-render completion registration, B-copy submission,
   KMS in-fence handoff, and page-flip retirement.
7. Integrate failure recovery, modeset/hotplug, VT suspend/resume, and shutdown.
8. Add pure state, fd routing, probe selection, failure injection, and Vulkan
   acceptance tests. Exercise a forced copied route on dual-GPU hardware.
9. Update `docs/status.md`, run `cargo +nightly fmt`, the workspace tests, and
   `cargo clippy --all-targets -- -D warnings`.
