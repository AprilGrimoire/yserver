# SUPERSEDED — see 2026-05-29-dri3-implicit-dmabuf-sync.md

This spec blamed `PresentPixmap` ignoring the client's `wait_fence`.
**Instrumentation refuted it:** on a blank Firefox run, all 3056 presents had
`wait_fence == 0` (`wait_triggered=no-fence`). There is no explicit fence to
honor — Firefox uses **implicit dma-buf sync**.

The real root cause and fix are in
**`2026-05-29-dri3-implicit-dmabuf-sync.md`**: yserver's DRI3 dma-buf import
(`kms/vk/target.rs::from_dmabuf`) does not export/wait on the buffer's
implicit producer fence before reading it, so `copy_area` reads while the
client's GPU may still be writing → intermittent blank.

(The general observation that yserver has no real fence-wait, and the
Present-drop WARN added as `fb8d388`, remain valid and are unrelated to this
particular symptom.)
