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

pub fn drain_events<F: FnMut(crtc::Handle, u32, std::time::Duration)>(
    device: &Device,
    mut on_page_flip: F,
) -> io::Result<()> {
    for event in device.receive_events()? {
        dispatch_event(event, &mut on_page_flip);
    }
    Ok(())
}

/// Dispatch a single drm event: invoke `on_page_flip` for `Event::PageFlip`
/// with the completing CRTC handle, the kernel vblank `frame` (MSC) and the
/// completion `duration` (UST timestamp); ignore `Vblank` and `Unknown`.
///
/// The MSC/UST are forwarded for Present vblank pacing — a compositor
/// (picom) drives its frame clock off `PresentNotifyMSC`, which must
/// complete with the real kernel `(msc, ust)` at each pageflip.
///
/// Factored out of [`drain_events`] so the per-event routing is unit-testable
/// without a real DRM fd (synthetic [`Event::PageFlip`] values can be
/// constructed via the public `PageFlipEvent` fields).
fn dispatch_event<F: FnMut(crtc::Handle, u32, std::time::Duration)>(
    event: Event,
    on_page_flip: &mut F,
) {
    if let Event::PageFlip(ev) = event {
        on_page_flip(ev.crtc, ev.frame, ev.duration);
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
            frame: 7,
            duration: Duration::from_micros(123),
            crtc: handle,
        });

        let mut seen: Vec<(crtc::Handle, u32, Duration)> = Vec::new();
        dispatch_event(event, &mut |c, frame, dur| seen.push((c, frame, dur)));

        assert_eq!(seen, vec![(handle, 7, Duration::from_micros(123))]);
    }

    #[test]
    fn dispatch_event_ignores_unknown() {
        let event = Event::Unknown(Vec::new());
        let mut called = 0u32;
        dispatch_event(event, &mut |_, _, _| called += 1);
        assert_eq!(called, 0);
    }
}
