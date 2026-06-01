//! Page-flip submission + completion drain.
//!
//! `submit_flip` atomic-commits a new FB_ID on the primary plane with
//! PAGE_FLIP_EVENT | NONBLOCK; the kernel produces a completion event
//! on the DRM fd when scanout latches the new buffer.
//!
//! `drain_events` reads pending events with `Device::receive_events()`
//! and dispatches PageFlip completions to a closure. The drm crate's
//! parser folds the kernel `user_data` field into `crtc` (preferring
//! `crtc_id` from the vblank event when present, else falling back to
//! `user_data`). The closure receives the per-CRTC handle so multi-output
//! callsites can route the completion to the right swapchain.

use std::io;

use drm::control::{
    AtomicCommitFlags, Device as ControlDevice, Event, atomic::AtomicModeReq, crtc, framebuffer,
};

use crate::drm::{
    Device,
    modeset::{Output, PropMap},
};

pub fn submit_flip(device: &Device, output: &Output, fb_id: framebuffer::Handle) -> io::Result<()> {
    submit_flip_inner(device, output, fb_id, None, None)
}

/// Atomic commit + explicit-fence flip (Phase 4.1.2.5). Used by the
/// Vulkan-fed scanout path: pass the SYNC_FD payload exported from
/// the bo's signalSemaphore as `in_fence_fd` so KMS waits for GPU
/// before scanning out, and pass `out_fence_holder` so the kernel
/// allocates a release fence we can wait on for retire.
///
/// The kernel takes ownership of `in_fence_fd` on a successful
/// commit (rc=0). On `-EBUSY` (or any other error) the caller still
/// owns the fd and must close it. `out_fence_holder` is written with
/// the new fence fd that the caller owns.
pub fn submit_flip_with_fences(
    device: &Device,
    output: &Output,
    fb_id: framebuffer::Handle,
    in_fence_fd: i32,
    out_fence_holder: &mut i32,
) -> io::Result<()> {
    submit_flip_inner(
        device,
        output,
        fb_id,
        Some(in_fence_fd),
        Some(out_fence_holder),
    )
}

fn submit_flip_inner(
    device: &Device,
    output: &Output,
    fb_id: framebuffer::Handle,
    in_fence_fd: Option<i32>,
    out_fence_holder: Option<&mut i32>,
) -> io::Result<()> {
    let mut req = AtomicModeReq::new();
    req.add_raw_property(
        output.plane.into(),
        output.plane_fb_id_prop,
        u64::from(u32::from(fb_id)),
    );
    req.add_raw_property(
        output.plane.into(),
        output.plane_crtc_id_prop,
        u64::from(u32::from(output.crtc)),
    );

    if let Some(fd) = in_fence_fd {
        // IN_FENCE_FD is a plane property. Its value is the fence fd
        // (sign-extended to u64; -1 means "no fence", which differs
        // from "absent").
        let prop = match output.plane_in_fence_fd_prop {
            Some(prop) => prop,
            None => PropMap::for_object(device, output.plane)?.id("IN_FENCE_FD")?,
        };
        req.add_raw_property(output.plane.into(), prop, fd as i64 as u64);
    }
    if let Some(holder) = out_fence_holder {
        // OUT_FENCE_PTR is a CRTC property. Its value is a userspace
        // pointer (cast to u64) where the kernel writes the freshly
        // allocated fence fd on a successful commit.
        let prop = match output.crtc_out_fence_ptr_prop {
            Some(prop) => prop,
            None => PropMap::for_object(device, output.crtc)?.id("OUT_FENCE_PTR")?,
        };
        let ptr_value = (holder as *mut i32) as usize as u64;
        req.add_raw_property(output.crtc.into(), prop, ptr_value);
    }

    device.atomic_commit(
        AtomicCommitFlags::PAGE_FLIP_EVENT | AtomicCommitFlags::NONBLOCK,
        req,
    )
}

pub fn drain_events<F: FnMut(crtc::Handle, u64, std::time::Duration)>(
    device: &Device,
    mut on_page_flip: F,
) -> io::Result<()> {
    for event in device.receive_events()? {
        dispatch_event(event, &mut on_page_flip);
    }
    Ok(())
}

/// T6 (idle-case MSC advance): request a vblank event from the
/// kernel so the next vblank arrives on the DRM fd even when no
/// pageflip is in flight. Mirrors Xorg
/// `present_screen_info::queue_vblank` semantics: ask the driver
/// for an event at the next vblank on the given CRTC; the run loop
/// drains the event via [`drain_events`] and uses the kernel
/// `(msc, ust)` to fire queued `CompleteNotify` / `NotifyMSC`
/// payloads.
///
/// `crtc_index` is the 0-based CRTC ordinal (0..32) — the kernel
/// encodes this into the `_DRM_VBLANK_HIGH_CRTC_MASK` bits. Higher
/// CRTC indices use the libdrm `high_crtc` field directly.
///
/// Returns the kernel-reported sequence the event was scheduled
/// for; the actual completion arrives asynchronously as
/// `Event::Vblank(crtc, frame, time)`.
pub fn request_next_vblank_event(device: &Device, crtc_index: u32) -> io::Result<()> {
    use drm::{Device as DrmDevice, VblankWaitFlags, VblankWaitTarget};
    // Relative(1) = "the next vblank from now"; with EVENT the
    // ioctl returns immediately and the completion lands on the
    // DRM fd, picked up by `drain_events` on the next epoll cycle.
    // `user_data` is opaque; we don't need it for routing because
    // the `VblankEvent` already carries `crtc`.
    device
        .wait_vblank(
            VblankWaitTarget::Relative(1),
            VblankWaitFlags::EVENT,
            crtc_index,
            0,
        )
        .map(drop)
}

/// Dispatch a single drm event: invoke `on_page_flip` for `Event::PageFlip`
/// or `Event::Vblank` with `(crtc, msc, ust)`; ignore `Unknown`.
///
/// `msc` is the kernel vblank sequence widened to u64 (raw sequence wraps
/// at u32; wrap-tracking is owned by the per-CRTC state in the backend).
/// `ust` is the kernel-reported timestamp (since-boot Duration, treated
/// as opaque UST by Present completion consumers).
///
/// T6 (idle-case MSC advance): Vblank events flow through the same
/// callback so the run loop can drain `pending_complete_notify` even
/// when no pageflip is happening. Vblanks are produced only when the
/// backend explicitly requests them via `wait_vblank` with the EVENT
/// flag (so idle sessions don't spin on vblanks); when no request is
/// in flight, the event stream is naturally PageFlip-only.
///
/// Factored out of [`drain_events`] so the per-event routing is unit-testable
/// without a real DRM fd (synthetic event values can be constructed via the
/// public `PageFlipEvent` / `VblankEvent` fields).
fn dispatch_event<F: FnMut(crtc::Handle, u64, std::time::Duration)>(
    event: Event,
    on_page_flip: &mut F,
) {
    match event {
        Event::PageFlip(ev) => {
            log::info!(
                "PRESENT-DBG: PageFlip event crtc={:?} msc={} dur={:?}",
                ev.crtc,
                ev.frame,
                ev.duration
            );
            on_page_flip(ev.crtc, u64::from(ev.frame), ev.duration);
        }
        Event::Vblank(ev) => {
            log::info!(
                "PRESENT-DBG: Vblank event crtc={:?} msc={} time={:?}",
                ev.crtc,
                ev.frame,
                ev.time
            );
            on_page_flip(ev.crtc, u64::from(ev.frame), ev.time);
        }
        Event::Unknown(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use drm::control::{Event, PageFlipEvent, crtc, from_u32};

    use super::dispatch_event;

    #[test]
    fn dispatch_event_passes_crtc_handle_for_page_flip() {
        let handle: crtc::Handle = from_u32(42).expect("non-zero raw handle");
        let event = Event::PageFlip(PageFlipEvent {
            frame: 0,
            duration: Duration::ZERO,
            crtc: handle,
        });

        let mut seen: Vec<crtc::Handle> = Vec::new();
        dispatch_event(event, &mut |c, _msc, _ust| seen.push(c));

        assert_eq!(seen, vec![handle]);
    }

    #[test]
    fn dispatch_event_ignores_unknown() {
        let event = Event::Unknown(Vec::new());
        let mut called = 0u32;
        dispatch_event(event, &mut |_, _, _| called += 1);
        assert_eq!(called, 0);
    }

    /// T6 (idle-case MSC advance): `Event::Vblank` must also reach the
    /// dispatch callback. The kernel emits Vblank when a `wait_vblank`
    /// (with `_DRM_VBLANK_EVENT`) target is reached even if no
    /// pageflip happened — that's the mechanism that keeps the MSC
    /// clock running when the compositor is idle. Pre-T6 the
    /// dispatcher dropped Vblank entirely, so `NotifyMSC` requests
    /// targeting future MSC values would never fire under an idle
    /// compositor (paint stalled when the cinnamon shell or Cogl
    /// frame clock asked "wake me at next vblank").
    #[test]
    fn dispatch_event_surfaces_vblank_event() {
        use drm::control::VblankEvent;

        let handle: crtc::Handle = from_u32(7).expect("non-zero raw handle");
        let ust = Duration::new(11, 250_000_000);
        let event = Event::Vblank(VblankEvent {
            frame: 9876,
            time: ust,
            crtc: handle,
            user_data: 0,
        });

        let mut seen: Vec<(crtc::Handle, u64, Duration)> = Vec::new();
        dispatch_event(event, &mut |c, msc, t| seen.push((c, msc, t)));

        assert_eq!(seen, vec![(handle, 9876u64, ust)]);
    }

    /// T1 (Present pacing): MSC + UST must reach the dispatch callback.
    /// `frame` widens to u64 (the kernel sequence is u32 — wrap handling
    /// is a separate concern owned by per-CRTC bookkeeping in T2).
    /// `duration` flows through as-is; consumers convert to a UST as
    /// needed.
    #[test]
    fn dispatch_event_surfaces_msc_and_ust_for_page_flip() {
        let handle: crtc::Handle = from_u32(42).expect("non-zero raw handle");
        let ust = Duration::new(2, 500_000_000);
        let event = Event::PageFlip(PageFlipEvent {
            frame: 12345,
            duration: ust,
            crtc: handle,
        });

        let mut seen: Vec<(crtc::Handle, u64, Duration)> = Vec::new();
        dispatch_event(event, &mut |c, msc, t| seen.push((c, msc, t)));

        assert_eq!(seen, vec![(handle, 12345u64, ust)]);
    }
}
