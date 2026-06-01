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

// ── DRM_IOCTL_CRTC_QUEUE_SEQUENCE plumbing ──────────────────────
//
// `drm` 0.15 / `drm-ffi` 0.9 do not wrap this ioctl; we issue it
// raw. Layouts mirror `<drm/drm.h>` exactly (kernel headers, verified
// against /usr/include/drm/drm.h on the build host). All multi-byte
// fields are little-endian on every supported target.
//
// Both flags are passed in the `flags` field; combined or'd.
pub(crate) const DRM_CRTC_SEQUENCE_RELATIVE: u32 = 0x0000_0001;
pub(crate) const DRM_CRTC_SEQUENCE_NEXT_ON_MISS: u32 = 0x0000_0002;

/// kernel `DRM_EVENT_CRTC_SEQUENCE` event type id.
pub(crate) const DRM_EVENT_CRTC_SEQUENCE: u32 = 0x03;

#[allow(non_camel_case_types)]
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct drm_crtc_queue_sequence {
    pub crtc_id: u32,
    pub flags: u32,
    /// In: target sequence. Out: actual scheduled sequence.
    pub sequence: u64,
    /// Echoed back verbatim in the resulting `drm_event_crtc_sequence`.
    pub user_data: u64,
}

#[allow(non_camel_case_types)]
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct drm_event_header {
    pub r#type: u32,
    pub length: u32,
}

#[allow(non_camel_case_types)]
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct drm_event_crtc_sequence {
    pub base: drm_event_header,
    pub user_data: u64,
    /// CLOCK_MONOTONIC nanoseconds. Signed per kernel header — we
    /// must `u64::try_from` rather than `as u64`.
    pub time_ns: i64,
    pub sequence: u64,
}

// `_IOWR('d', 0x3C, drm_crtc_queue_sequence)` expanded inline so
// the request code is a `const` we can also assert in a unit test.
//   dir = 3 (RW), type = 'd' (0x64), nr = 0x3C, size = 24
pub(crate) const DRM_IOCTL_CRTC_QUEUE_SEQUENCE: libc::c_ulong = ((3 as libc::c_ulong) << 30)
    | ((std::mem::size_of::<drm_crtc_queue_sequence>() as libc::c_ulong) << 16)
    | ((0x64 as libc::c_ulong) << 8)
    | 0x3C;

/// Queue a one-shot CRTC vblank sequence event. `crtc_id` is the
/// **raw KMS object id** (NOT a pipe index — that distinction is
/// the whole reason this helper exists; the legacy `drmWaitVBlank`
/// path used pipe indices and lost the dual-monitor case).
///
/// - `relative = true`  → kernel arms `current_msc + sequence`
///   vblanks from now; pass `sequence = 1` for "next vblank".
///   `NEXT_ON_MISS` is set in both modes (the kernel only acts on it
///   for absolute targets — harmless on the relative path).
/// - `relative = false` → absolute target. **Always pair with
///   `NEXT_ON_MISS`** (set internally) so an already-passed target
///   fires at the next vblank instead of waiting a full 32-bit
///   counter wrap.
///
/// `user_data` is echoed verbatim in the resulting
/// `DRM_EVENT_CRTC_SEQUENCE` — we encode the stable `crtc_id` there
/// (NOT `output_idx`, which is unstable across hotplug compaction
/// in `v2/platform.rs::requery_outputs_and_modeset`).
///
/// Returns the kernel-assigned scheduled sequence on success.
///
/// # Errors
///
/// - `EOPNOTSUPP` on pre-4.14 kernels — caller should fall back
///   to the relative-keep-alive path.
/// - `EACCES` if we no longer hold DRM master — caller must have
///   pre-gated on `scanout_allowed()`.
// Caller wired in Task 8; suppress dead_code until then.
#[allow(dead_code)]
pub(crate) fn queue_crtc_sequence(
    device: &Device,
    crtc_id: u32,
    relative: bool,
    sequence: u64,
    user_data: u64,
) -> io::Result<u64> {
    use std::os::{fd::AsFd, unix::io::AsRawFd};

    let mut flags = DRM_CRTC_SEQUENCE_NEXT_ON_MISS;
    if relative {
        flags |= DRM_CRTC_SEQUENCE_RELATIVE;
    }
    let mut req = drm_crtc_queue_sequence {
        crtc_id,
        flags,
        sequence,
        user_data,
    };
    // SAFETY: `req` is a fully-initialised POD of the exact size the
    // kernel expects (24 bytes — pinned by the unit tests in Task 1).
    // No other reference to `req` exists across the syscall boundary
    // (it is stack-local and only borrowed once as `&mut`). The device
    // fd is held alive by `device` for the duration of the call; the
    // kernel reads and writes `req` in place.
    let raw_fd = device.as_fd().as_raw_fd();
    let rc = unsafe { libc::ioctl(raw_fd, DRM_IOCTL_CRTC_QUEUE_SEQUENCE, &mut req as *mut _) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(req.sequence)
}

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

pub fn drain_events<A, S>(device: &Device, mut on_advance: A, mut on_sequence: S) -> io::Result<()>
where
    A: FnMut(crtc::Handle, u64, std::time::Duration),
    S: FnMut(u32, i64, u64),
{
    for event in device.receive_events()? {
        dispatch_event(event, &mut on_advance, &mut on_sequence);
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

/// Dispatch a single drm event.
///
/// - `Event::PageFlip` / `Event::Vblank` → `on_advance(crtc, msc, ust)`
///   (existing behaviour; carries kernel `(msc, ust)` already widened).
///   Note: under the EOPNOTSUPP fallback path (relative-1 keep-alive,
///   wired in Task 8), the kernel emits `Event::Vblank` rather than
///   a `CRTC_SEQUENCE` event — both flow through `on_advance` and
///   into the same `record_crtc_ust_msc` → arm-clear pipeline.
/// - `Event::Unknown` matching `DRM_EVENT_CRTC_SEQUENCE` (type==3,
///   length==32) → `on_sequence(crtc_id_raw_u32, time_ns_i64,
///   sequence_u64)`. **Raw**: `time_ns` is signed and not yet
///   validated; `crtc_id_raw` is the bottom 32 bits of `user_data`
///   (we encode it there in Task 8). The caller does clear-arm
///   BEFORE any drop on validity check.
/// - Everything else: dropped.
///
/// Factored so per-event routing is unit-testable without a real DRM fd.
fn dispatch_event<A, S>(event: Event, on_advance: &mut A, on_sequence: &mut S)
where
    A: FnMut(crtc::Handle, u64, std::time::Duration),
    S: FnMut(u32, i64, u64),
{
    match event {
        Event::PageFlip(ev) => {
            log::info!(
                "PRESENT-DBG: PageFlip event crtc={:?} msc={} dur={:?}",
                ev.crtc,
                ev.frame,
                ev.duration
            );
            on_advance(ev.crtc, u64::from(ev.frame), ev.duration);
        }
        Event::Vblank(ev) => {
            log::info!(
                "PRESENT-DBG: Vblank event crtc={:?} msc={} time={:?}",
                ev.crtc,
                ev.frame,
                ev.time
            );
            on_advance(ev.crtc, u64::from(ev.frame), ev.time);
        }
        Event::Unknown(bytes) => {
            // Header: u32 type, u32 length (8 bytes total).
            if bytes.len() < std::mem::size_of::<drm_event_header>() {
                return;
            }
            // SAFETY: bytes.len() >= size_of::<drm_event_header>()
            // (guarded above); drm_event_header is #[repr(C)] Copy
            // with no padding; bytes is a local Vec with no
            // concurrent aliases.
            let header: drm_event_header =
                unsafe { std::ptr::read_unaligned(bytes.as_ptr() as *const drm_event_header) };
            if header.r#type != DRM_EVENT_CRTC_SEQUENCE {
                return;
            }
            if header.length as usize != std::mem::size_of::<drm_event_crtc_sequence>() {
                return;
            }
            if bytes.len() < std::mem::size_of::<drm_event_crtc_sequence>() {
                return;
            }
            // SAFETY: bytes.len() >= size_of::<drm_event_crtc_sequence>()
            // (guarded above); drm_event_crtc_sequence is #[repr(C)]
            // Copy with no padding (pinned by
            // drm_event_crtc_sequence_struct_is_32_bytes); bytes is a
            // local Vec with no concurrent aliases.
            let ev: drm_event_crtc_sequence = unsafe {
                std::ptr::read_unaligned(bytes.as_ptr() as *const drm_event_crtc_sequence)
            };
            // Bottom 32 bits of user_data are the crtc_id we encoded.
            #[allow(clippy::cast_possible_truncation)]
            let crtc_id_raw = ev.user_data as u32;
            log::info!(
                "PRESENT-DBG: CrtcSequence event crtc_id={crtc_id_raw} sequence={} time_ns={}",
                ev.sequence,
                ev.time_ns
            );
            on_sequence(crtc_id_raw, ev.time_ns, ev.sequence);
        }
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
        dispatch_event(event, &mut |c, _msc, _ust| seen.push(c), &mut |_, _, _| {});

        assert_eq!(seen, vec![handle]);
    }

    #[test]
    fn dispatch_event_ignores_unknown() {
        let event = Event::Unknown(Vec::new());
        let mut called = 0u32;
        dispatch_event(event, &mut |_, _, _| called += 1, &mut |_, _, _| {});
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
        dispatch_event(
            event,
            &mut |c, msc, t| seen.push((c, msc, t)),
            &mut |_, _, _| {},
        );

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
        dispatch_event(
            event,
            &mut |c, msc, t| seen.push((c, msc, t)),
            &mut |_, _, _| {},
        );

        assert_eq!(seen, vec![(handle, 12345u64, ust)]);
    }

    #[test]
    fn drm_crtc_queue_sequence_struct_is_24_bytes() {
        // Header drm.h line 1064: __u32 crtc_id; __u32 flags;
        // __u64 sequence; __u64 user_data; → 4+4+8+8 = 24 bytes.
        assert_eq!(std::mem::size_of::<super::drm_crtc_queue_sequence>(), 24);
        assert_eq!(std::mem::align_of::<super::drm_crtc_queue_sequence>(), 8);
    }

    #[test]
    fn drm_event_crtc_sequence_struct_is_32_bytes() {
        // Header drm.h line 1429: struct drm_event base (8B) +
        // __u64 user_data + __s64 time_ns + __u64 sequence
        // = 8 + 8 + 8 + 8 = 32. (The spec note "24 bytes" refers
        // to the payload AFTER the 8-byte drm_event header; sizeof
        // of the full struct including header is 32.)
        assert_eq!(std::mem::size_of::<super::drm_event_crtc_sequence>(), 32);
        assert_eq!(std::mem::align_of::<super::drm_event_crtc_sequence>(), 8);
    }

    #[test]
    fn queue_sequence_layout_absolute_with_next_on_miss() {
        use super::{DRM_CRTC_SEQUENCE_NEXT_ON_MISS, drm_crtc_queue_sequence};
        // Absolute target: flags == NEXT_ON_MISS only (no RELATIVE bit).
        let req = drm_crtc_queue_sequence {
            crtc_id: 0x42,
            flags: DRM_CRTC_SEQUENCE_NEXT_ON_MISS,
            sequence: 0x1234_5678_9ABC_DEF0,
            user_data: 0x42,
        };
        assert_eq!(
            req.flags & 1,
            0,
            "RELATIVE bit must be clear for absolute target"
        );
        assert_eq!(req.flags & 2, 2, "NEXT_ON_MISS must be set");
    }

    #[test]
    fn queue_sequence_layout_relative_one() {
        use super::{DRM_CRTC_SEQUENCE_RELATIVE, drm_crtc_queue_sequence};
        // target_msc == 0 maps to RELATIVE | sequence=1.
        let req = drm_crtc_queue_sequence {
            crtc_id: 0x42,
            flags: DRM_CRTC_SEQUENCE_RELATIVE,
            sequence: 1,
            user_data: 0x42,
        };
        assert_eq!(req.flags & 1, 1);
        assert_eq!(req.sequence, 1);
    }

    #[test]
    fn drm_crtc_queue_sequence_ioctl_request_code() {
        // _IOWR('d' /*0x64*/, 0x3C, drm_crtc_queue_sequence).
        // _IOC(dir=3 /*RW*/, type='d', nr=0x3C, size=24)
        //   = (3 << 30) | (24 << 16) | (0x64 << 8) | 0x3C
        //   = 0xC018_643C. Compute:
        //     (3 << 30) = 0xC0000000
        //     (24 << 16) = 0x00180000
        //     (0x64 << 8) = 0x00006400
        //     0x3C = 0x3C
        //   = 0xC018643C
        assert_eq!(
            super::DRM_IOCTL_CRTC_QUEUE_SEQUENCE,
            0xC018_643C as libc::c_ulong
        );
    }

    #[test]
    fn dispatch_event_decodes_crtc_sequence() {
        use super::{DRM_EVENT_CRTC_SEQUENCE, drm_event_crtc_sequence, drm_event_header};
        // Build a raw 32-byte event matching the kernel layout.
        let raw = drm_event_crtc_sequence {
            base: drm_event_header {
                r#type: DRM_EVENT_CRTC_SEQUENCE,
                length: 32,
            },
            user_data: 0xCAFE_BABE_0000_0042, // bottom 32 bits = crtc_id 0x42
            time_ns: 1_234_567_890_i64,
            sequence: 9_999,
        };
        let bytes: [u8; 32] = unsafe { std::mem::transmute(raw) };
        let event = Event::Unknown(bytes.to_vec());

        let mut advance_calls = Vec::<(crtc::Handle, u64, Duration)>::new();
        let mut seq_calls = Vec::<(u32, i64, u64)>::new();
        super::dispatch_event(
            event,
            &mut |c, m, u| advance_calls.push((c, m, u)),
            &mut |cid, t, s| seq_calls.push((cid, t, s)),
        );

        assert!(
            advance_calls.is_empty(),
            "sequence event must NOT route through advance callback"
        );
        assert_eq!(seq_calls.len(), 1);
        let (cid, time_ns, seq) = seq_calls[0];
        // Bottom 32 bits of user_data carry the crtc_id.
        assert_eq!(cid, 0x42);
        assert_eq!(time_ns, 1_234_567_890_i64);
        assert_eq!(seq, 9_999);
    }

    #[test]
    fn dispatch_event_ignores_wrong_length_sequence_event() {
        use super::{DRM_EVENT_CRTC_SEQUENCE, drm_event_header};
        // Right type, wrong header.length → silently dropped.
        // Use a FULL-SIZED 32-byte buffer so guard 4 (bytes.len()
        // < 32) cannot fire; this isolates guard 3 (header.length
        // != size_of::<drm_event_crtc_sequence>()).
        let header = drm_event_header {
            r#type: DRM_EVENT_CRTC_SEQUENCE,
            length: 16,
        };
        let mut bytes = vec![0u8; 32];
        bytes[..8].copy_from_slice(&unsafe { std::mem::transmute::<_, [u8; 8]>(header) });
        let event = Event::Unknown(bytes);

        let mut advance_calls = 0usize;
        let mut seq_calls = 0usize;
        super::dispatch_event(event, &mut |_, _, _| advance_calls += 1, &mut |_, _, _| {
            seq_calls += 1
        });
        assert_eq!(advance_calls, 0);
        assert_eq!(seq_calls, 0);
    }

    #[test]
    fn dispatch_event_ignores_unknown_type() {
        use super::drm_event_header;
        let header = drm_event_header {
            r#type: 99,
            length: 32,
        };
        let mut bytes = vec![0u8; 32];
        bytes[..8].copy_from_slice(&unsafe { std::mem::transmute::<_, [u8; 8]>(header) });
        let event = Event::Unknown(bytes);
        let mut seq_calls = 0usize;
        super::dispatch_event(event, &mut |_, _, _| {}, &mut |_, _, _| seq_calls += 1);
        assert_eq!(seq_calls, 0);
    }
}
