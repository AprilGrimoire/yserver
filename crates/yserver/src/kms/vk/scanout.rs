//! Per-bo state machine + Vulkan-first scanout-bo allocation
//! (sub-phase 4.1.2).
//!
//! Spec: docs/superpowers/specs/2026-05-07-phase4-1-vulkan-compositor-design.md
//! §"Per-buffer release fence" — table of transitions and fence-handle
//! ownership rules.
//!
//! ## Allocation direction
//!
//! Vulkan-first: each [`ScanoutBo`] owns a dma-buf-exportable
//! `VkImage`. The preferred path allocates it with an explicit DRM
//! format modifier from the KMS primary plane / Vulkan intersection;
//! fallback paths keep the historical `VK_IMAGE_TILING_LINEAR` image.
//! The memory is exported as a dma-buf via `vkGetMemoryFdKHR`, the
//! dma-buf is imported into the DRM device via `PRIME_FD_TO_HANDLE`,
//! and the resulting GEM handle is registered as a DRM framebuffer
//! with `add_fb2`.
//!
//! Why Vulkan-first instead of GBM-first:
//! - Works on RADV gfx8/Polaris (no `VK_EXT_image_drm_format_modifier`
//!   needed; linear images don't need a modifier handle to communicate
//!   to KMS).
//! - Works through Mesa Venus (host-allocated memory wrapped as a
//!   virtgpu blob → guest dma-buf → guest virtio-gpu DRM. Same
//!   direction Mesa+Wayland use; the reverse direction
//!   `vkGetMemoryResourcePropertiesMESA`'d on a guest GBM bo aborts
//!   the Venus driver).
//! - One renderer; no driver-specific skip paths inside the bo
//!   constructor.

use std::{
    io,
    os::fd::{AsFd, FromRawFd, OwnedFd},
    rc::Rc,
    sync::Arc,
};

use ash::vk;
use drm::{
    Device as _, DriverCapability,
    buffer::{DrmFourcc, DrmModifier, Handle as DrmBufferHandle, PlanarBuffer as DrmPlanarBuffer},
    control::{Device as DrmControlDevice, FbCmd2Flags, framebuffer},
};

use super::device::VkContext;
use crate::kms::backend::ScanoutRoute;

/// Per-bo phase. The lifecycle is roughly
/// `Free → Recording → Submitted → Pending → OnScreen → Retiring → Free`.
/// `Submitted` can also revert to `Recording` on atomic-EBUSY, or jump
/// to `Free` on modeset preempt.
#[derive(Debug, Default, PartialEq, Eq, Clone, Copy)]
pub enum BoPhase {
    /// Not in flight; GPU may write into it. No fences attached.
    #[default]
    Free,
    /// Composite CB being recorded for this bo.
    Recording,
    /// `vkQueueSubmit2` issued; we still own the `IN_FENCE_FD` until
    /// the atomic commit either accepts (kernel consumes it) or
    /// rejects (we close it).
    Submitted,
    /// `drmModeAtomicCommit` accepted; `IN_FENCE_FD` ownership
    /// transferred to kernel; we now own the `OUT_FENCE_FD` (the
    /// release fence).
    Pending,
    /// Pageflip-complete arrived. Bo is on-screen. The release fence
    /// is signal-pending (KMS signals it when the next flip retires
    /// this bo).
    OnScreen,
    /// A later flip's pageflip-complete arrived; this bo is no
    /// longer on screen. Release fence is signalled. Returns to
    /// `Free` once all GPU readers (e.g. damage-diff sources)
    /// complete.
    Retiring,
}

/// Fence-fd handles + the current phase. Owns no DRM/Vulkan state
/// directly — callers thread the actual `VkImage` / framebuffer
/// alongside.
#[derive(Debug, Default)]
pub struct BoState {
    pub phase: BoPhase,
    /// Fence we exported from `vkGetSemaphoreFdKHR` after submit and
    /// will pass to KMS as `IN_FENCE_FD`. We own it until the kernel
    /// consumes it on atomic accept.
    pub in_fence_fd: Option<i32>,
    /// Fence the kernel allocated and handed back via `OUT_FENCE_PTR`.
    /// Signalled when the next flip retires this bo.
    pub release_fence_fd: Option<i32>,
}

impl BoState {
    /// `Free → Recording`: acquire for next frame's render target.
    pub fn transition_to_recording(&mut self) {
        debug_assert_eq!(self.phase, BoPhase::Free);
        self.phase = BoPhase::Recording;
    }

    /// `Recording → Submitted`: `vkQueueSubmit2` issued. Caller
    /// already exported `IN_FENCE_FD` and passes it in.
    pub fn transition_to_submitted(&mut self, in_fence_fd: i32) {
        debug_assert_eq!(self.phase, BoPhase::Recording);
        self.phase = BoPhase::Submitted;
        self.in_fence_fd = (in_fence_fd >= 0).then_some(in_fence_fd);
    }

    /// `Submitted → Pending`: atomic accepted. Returns the in-fence
    /// fd so the caller can close it (the kernel takes a reference
    /// to the underlying `sync_file` during the commit but does NOT
    /// own the fd — userspace must close it). Adopts the out-fence
    /// fd from KMS as the release fence.
    #[must_use = "in-fence fd must be closed by the caller"]
    pub fn transition_to_pending(&mut self, out_fence_fd: i32) -> Option<i32> {
        debug_assert_eq!(self.phase, BoPhase::Submitted);
        self.phase = BoPhase::Pending;
        let in_fence = self.in_fence_fd.take();
        self.release_fence_fd = (out_fence_fd >= 0).then_some(out_fence_fd);
        in_fence
    }

    /// `Submitted → Recording`: atomic returned `-EBUSY`. Caller is
    /// responsible for closing the returned in-fence fd.
    pub fn transition_to_recording_after_atomic_reject(&mut self) -> Option<i32> {
        debug_assert_eq!(self.phase, BoPhase::Submitted);
        self.phase = BoPhase::Recording;
        self.in_fence_fd.take()
    }

    /// `Submitted → Free`: modeset preempts (CRTC reconfigure
    /// mid-flight). Caller has already host-waited on the in-flight
    /// GPU work and must close the returned fd.
    pub fn transition_to_free_after_modeset_preempt(&mut self) -> Option<i32> {
        debug_assert_eq!(self.phase, BoPhase::Submitted);
        self.phase = BoPhase::Free;
        self.in_fence_fd.take()
    }

    /// `Pending → OnScreen`: first pageflip-complete event for this
    /// bo arrived.
    pub fn transition_to_on_screen(&mut self) {
        debug_assert_eq!(self.phase, BoPhase::Pending);
        self.phase = BoPhase::OnScreen;
    }

    /// `OnScreen → Retiring`: next flip's pageflip-complete arrived.
    /// Release fence is now signal-pending (will be signalled by
    /// KMS).
    pub fn transition_to_retiring(&mut self) {
        debug_assert_eq!(self.phase, BoPhase::OnScreen);
        self.phase = BoPhase::Retiring;
    }

    /// `Retiring → Free`: all GPU readers are done; caller will close
    /// the returned release fence fd.
    pub fn transition_to_free_after_retire(&mut self) -> Option<i32> {
        debug_assert_eq!(self.phase, BoPhase::Retiring);
        self.phase = BoPhase::Free;
        self.release_fence_fd.take()
    }

    /// `any → Free` on modeset reset (hotunplug, mode change). Caller
    /// must close every returned fd. The two slots may both be
    /// populated if the bo was Submitted-then-immediately-Pending
    /// somehow; in normal flow only one is.
    pub fn transition_to_free_after_modeset_reset(&mut self) -> ModesetReleased {
        let in_fence = self.in_fence_fd.take();
        let release_fence = self.release_fence_fd.take();
        self.phase = BoPhase::Free;
        ModesetReleased {
            in_fence,
            release_fence,
        }
    }
}

/// Fences released when a bo is force-reset on modeset. Caller closes
/// each `Some(fd)` exactly once.
#[derive(Debug)]
pub struct ModesetReleased {
    pub in_fence: Option<i32>,
    pub release_fence: Option<i32>,
}

/// One scanout buffer object: a Vulkan-allocated `VkImage` exported
/// as a dma-buf and imported into the DRM device for KMS scanout.
///
/// All fields are populated after `allocate()` returns successfully.
/// Drop unwinds them in the right order (DRM framebuffer → GEM handle
/// close → VkImage → memory → semaphore → command pool).
#[allow(dead_code)] // most fields used by 4.1.2.5+ atomic-commit driver.
pub struct ScanoutBo {
    pub state: BoState,
    pub width: u32,
    pub height: u32,
    /// `true` for client-imported alien BOs (Phase 4.2.4 Flip /
    /// DirectScanout); `false` for pool-allocated server BOs. Alien
    /// BOs share the framebuffer-registration code path but skip the
    /// allocator: they're wired in by `ScanoutBoPool::register_alien`.
    pub is_alien: bool,
    /// Row pitch in bytes — what the driver chose for our
    /// `TILING_LINEAR` image. Passed to KMS as `pitch[0]` and to the
    /// blit copy as the destination row stride.
    pub pitch: u32,
    pub vk_image: vk::Image,
    pub vk_memory: vk::DeviceMemory,
    /// Color image view bound by the composite pass's
    /// `vkCmdBeginRendering` as the color attachment. Lives as long
    /// as `vk_image`. Built lazily on first use to avoid forcing
    /// every PixmanShadow-only deployment to allocate a view it
    /// never reads.
    pub vk_image_view: vk::ImageView,
    /// Long-lived binary semaphore used as `signalSemaphore` on the
    /// per-frame composite submit. Its payload is exported as a
    /// SYNC_FD after every submit and handed to KMS as `IN_FENCE_FD`.
    /// Object reused for the bo's whole lifetime; only the fd
    /// payload churns.
    pub vk_semaphore: vk::Semaphore,
    /// DRM framebuffer registered against this bo's GEM handle.
    /// `Option` so Drop can take it.
    pub fb_handle: Option<framebuffer::Handle>,
    /// GEM handle from `PRIME_FD_TO_HANDLE`. Closed via `GEM_CLOSE`
    /// in Drop. `Option` so Drop can take it.
    pub gem_handle: Option<DrmBufferHandle>,
    /// Per-bo transfer resources: command pool + a single command
    /// buffer recycled across frames, a host-mapped staging buffer
    /// sized for the bo (XRGB8888 → 4 bytes × width × height), and
    /// the device memory backing it.
    pub vk_transfer: TransferResources,
    /// Shared DRM device handle (for un-registering the framebuffer
    /// + closing the GEM handle in Drop).
    drm: Rc<crate::drm::Device>,
    /// Held to keep image+memory destructors anchored to a live
    /// device. Cloned per bo from the pool's Arc so individual bos
    /// can be moved/dropped independently.
    vk: Arc<VkContext>,
    /// When `true`, `Drop` early-returns: no explicit
    /// `destroy_framebuffer`, no GEM close, no Vk teardown.
    /// Resources are then leaked until process-exit DRM-fd close +
    /// VkDevice teardown — the kernel reaps GEM/FB on device-fd close
    /// and the userspace heap goes away with the process. This is a
    /// deliberate last-resort leak path, not a normal cleanup route.
    /// Set by `disarm()` from the shutdown path when atomic
    /// `disable_output` failed for this BO's CRTC — KMS may still
    /// hold the FB, so user-side teardown would corrupt kernel state.
    ///
    /// **ONLY safe to use at final process exit.** This Drop
    /// short-circuit bypasses Vk image / memory / GEM / FB cleanup
    /// but does NOT prevent Rust from dropping other fields (like
    /// the `Arc<VkContext>`). Using disarm at runtime (hotplug,
    /// modeset recovery) could produce a zombie VkImage when the
    /// VkContext's refcount expires.
    disarmed: bool,
}

/// Per-bo transfer-side resources (command pool/buffer + staging
/// buffer).
#[allow(dead_code)] // exercised by 4.1.2.5 atomic-commit driver.
pub struct TransferResources {
    pub command_pool: vk::CommandPool,
    pub command_buffer: vk::CommandBuffer,
    pub staging_buffer: vk::Buffer,
    pub staging_memory: vk::DeviceMemory,
    pub staging_mapped: std::ptr::NonNull<u8>,
    pub staging_size: u64,
}

// `NonNull<u8>` isn't `Send` by default. ScanoutBo is single-thread
// (KmsBackend is single-thread; Backend trait is used from one
// thread). Once 4.1.3+ adds threading, revisit.
unsafe impl Send for TransferResources {}
unsafe impl Sync for TransferResources {}

/// One pool per CRTC; holds N bos that rotate through the state
/// machine. Three is the documented sweet spot (design §2): one
/// scanning out, one queued, one being recorded into.
#[allow(dead_code)] // wired in via KmsBackend in a later commit.
pub struct ScanoutBoPool {
    pub bos: Vec<ScanoutBo>,
    pub width: u32,
    pub height: u32,
    /// Device-qualified ownership of the images and DRM framebuffers in
    /// `bos`. Every BO in a pool uses the same pair of endpoints.
    pub(crate) route: ScanoutRoute,
    /// Metadata-only result captured before any BO in this pool was
    /// exported. This is a necessary-capability check, not proof that a
    /// future PRIME import ioctl will succeed on every allocation.
    pub(crate) compatibility: DmabufScanoutCompatibility,
}

/// How a Vulkan linear image may be registered on the KMS endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LinearScanoutSupport {
    /// The metadata probe found no shared linear path.
    None,
    /// KMS advertised `DRM_FORMAT_MOD_LINEAR` in the plane's `IN_FORMATS`.
    ExplicitModifier,
    /// KMS exposed no `IN_FORMATS`; only the traditional untagged `addfb2`
    /// linear path can be attempted.
    LegacyAddfb,
}

/// Why metadata proves that a dma-buf route cannot work through yserver's
/// current Vulkan-first allocator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DmabufScanoutIncompatibility {
    /// The selected Vulkan logical device cannot export DMA_BUF memory.
    VulkanExternalMemoryFdUnavailable,
    /// Vulkan positively mapped the route's render DRM device to another
    /// physical device, so this context cannot own the advertised source.
    RenderDeviceMismatch,
    /// The sink DRM driver positively reported no PRIME import capability.
    KmsPrimeImportUnsupported,
}

/// Metadata that was unavailable or insufficient to prove compatibility.
/// These conditions must not suppress the authoritative runtime import.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DmabufScanoutUncertainty {
    /// Vulkan exposed no DRM identity for the route's render device.
    RenderDeviceIdentityUnavailable,
    /// Querying the sink's `DRM_CAP_PRIME` capability failed.
    KmsPrimeImportCapabilityUnavailable,
    /// Advertised Vulkan/KMS formats did not establish a shared modifier or
    /// linear path; drivers may still accept the historical fallback.
    NoAdvertisedSharedLayout,
}

/// Tri-state conclusion of the metadata-only compatibility probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DmabufScanoutVerdict {
    /// All required metadata was present and described a usable route.
    Compatible,
    /// Metadata conclusively rules out the current allocation path.
    Incompatible(DmabufScanoutIncompatibility),
    /// Metadata was incomplete; allocation must still be attempted.
    Unknown(Vec<DmabufScanoutUncertainty>),
}

impl DmabufScanoutVerdict {
    /// Only conclusive incompatibility suppresses the runtime import attempt.
    fn blocks_attempt(&self) -> bool {
        matches!(self, Self::Incompatible(_))
    }
}

/// Necessary metadata capabilities for one Vulkan-to-KMS dma-buf route.
///
/// `modifiers` is the ordered intersection of KMS plane modifiers and Vulkan
/// single-plane exportable modifiers. `linear` records the separate
/// `VK_IMAGE_TILING_LINEAR` fallback. `verdict` combines device identity,
/// external-memory, PRIME-import, and layout evidence without reducing
/// unavailable metadata to `false`. No probe exports a dma-buf or invokes
/// `PRIME_FD_TO_HANDLE`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DmabufScanoutCompatibility {
    pub(crate) route: ScanoutRoute,
    pub(crate) modifiers: Vec<u64>,
    pub(crate) linear: LinearScanoutSupport,
    pub(crate) verdict: DmabufScanoutVerdict,
}

impl ScanoutBo {
    /// Allocate one Vulkan-first scanout bo: VkImage + dma-buf-
    /// exportable memory + DRM framebuffer registration.
    /// All steps must succeed; partial allocations are unwound on
    /// error so the returned `Err` leaves no resources leaked.
    fn allocate(
        vk: Arc<VkContext>,
        drm: Rc<crate::drm::Device>,
        width: u32,
        height: u32,
        plans: &[ScanoutAllocationPlan],
    ) -> io::Result<Self> {
        let mut errors = Vec::new();

        for &plan in plans {
            match Self::allocate_with_plan(vk.clone(), Rc::clone(&drm), width, height, plan) {
                Ok(bo) => {
                    log::info!(
                        "scanout bo: {} succeeded ({}x{}, pitch {})",
                        plan.describe(),
                        width,
                        height,
                        bo.pitch,
                    );
                    return Ok(bo);
                }
                Err(e) => {
                    errors.push(format!("{}: {e}", plan.describe()));
                }
            }
        }

        Err(io::Error::other(format!(
            "scanout allocation failed for every path: {}",
            errors.join("; ")
        )))
    }

    fn allocate_with_plan(
        vk: Arc<VkContext>,
        drm: Rc<crate::drm::Device>,
        width: u32,
        height: u32,
        plan: ScanoutAllocationPlan,
    ) -> io::Result<Self> {
        // 1. VkImage + memory + dma-buf export.
        let img = allocate_vk_scanout_image(&vk, width, height, plan)
            .map_err(|e| io::Error::other(format!("vk scanout image: {e}")))?;
        let VkScanoutImage {
            image,
            memory,
            dmabuf,
            pitch,
            modifier,
        } = img;

        // 2. PRIME_FD_TO_HANDLE on the DRM device.
        let gem_handle = match drm.prime_fd_to_buffer(dmabuf.as_fd()) {
            Ok(h) => h,
            Err(e) => {
                destroy_scanout_image(&vk, image, memory);
                return Err(io::Error::other(format!("drm prime_fd_to_buffer: {e}")));
            }
        };
        // The GEM handle holds its own reference; close the dma-buf
        // fd we no longer need.
        drop(dmabuf);

        // 3. add_fb2. Modifier-backed paths must pass the MODIFIERS
        // flag even for DRM_FORMAT_MOD_LINEAR; the legacy fallback
        // deliberately keeps the old untagged shape.
        let fb_handle = match drm.add_planar_framebuffer(
            &VkScanoutFb {
                gem_handle,
                width,
                height,
                pitch,
                modifier,
            },
            addfb_flags_for_modifier(modifier),
        ) {
            Ok(h) => h,
            Err(e) => {
                let _ = drm.close_buffer(gem_handle);
                destroy_scanout_image(&vk, image, memory);
                return Err(io::Error::other(format!("drm add_fb: {e}")));
            }
        };

        // 4. Long-lived export semaphore.
        let vk_semaphore = match create_export_semaphore(&vk) {
            Ok(s) => s,
            Err(e) => {
                let _ = drm.destroy_framebuffer(fb_handle);
                let _ = drm.close_buffer(gem_handle);
                unsafe {
                    vk.device.destroy_image(image, None);
                    vk.device.free_memory(memory, None);
                }
                return Err(io::Error::other(format!("vk semaphore: {e}")));
            }
        };

        // 5. Per-bo transfer resources (always present now —
        //    every bo has a live VkImage to upload into).
        let vk_transfer = match allocate_transfer_resources(&vk, width, height) {
            Ok(t) => t,
            Err(e) => {
                unsafe {
                    vk.device.destroy_semaphore(vk_semaphore, None);
                    vk.device.destroy_image(image, None);
                    vk.device.free_memory(memory, None);
                }
                let _ = drm.destroy_framebuffer(fb_handle);
                let _ = drm.close_buffer(gem_handle);
                return Err(io::Error::other(format!("vk transfer: {e}")));
            }
        };

        // 6. Color image view used by the 4.1.3.4 composite pass
        //    `vkCmdBeginRendering` as the color attachment.
        let view_info = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(vk::Format::B8G8R8A8_UNORM)
            .subresource_range(
                vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .level_count(1)
                    .layer_count(1),
            );
        let vk_image_view = match unsafe { vk.device.create_image_view(&view_info, None) } {
            Ok(v) => v,
            Err(e) => {
                unsafe {
                    vk.device.unmap_memory(vk_transfer.staging_memory);
                    vk.device.destroy_buffer(vk_transfer.staging_buffer, None);
                    vk.device.free_memory(vk_transfer.staging_memory, None);
                    vk.device
                        .destroy_command_pool(vk_transfer.command_pool, None);
                    vk.device.destroy_semaphore(vk_semaphore, None);
                    vk.device.destroy_image(image, None);
                    vk.device.free_memory(memory, None);
                }
                let _ = drm.destroy_framebuffer(fb_handle);
                let _ = drm.close_buffer(gem_handle);
                return Err(io::Error::other(format!("vk image view: {e}")));
            }
        };

        Ok(Self {
            state: BoState::default(),
            width,
            height,
            is_alien: false,
            pitch,
            vk_image: image,
            vk_memory: memory,
            vk_image_view,
            vk_semaphore,
            fb_handle: Some(fb_handle),
            gem_handle: Some(gem_handle),
            vk_transfer,
            drm,
            vk,
            disarmed: false,
        })
    }

    /// Export a SYNC_FD payload from this bo's signal semaphore. Call
    /// this after `vkQueueSubmit2` with `signalSemaphore = vk_semaphore`
    /// — it returns the freshly-payloaded fd to hand KMS as
    /// `IN_FENCE_FD`. `None` maps to the KMS `-1` no-fence sentinel.
    #[allow(dead_code)] // wired in by Task 2.5 (atomic-commit fence path).
    pub fn export_signaled_fd(&self) -> Result<Option<OwnedFd>, vk::Result> {
        let ext = self.vk.external_semaphore_fd.clone();
        let info = vk::SemaphoreGetFdInfoKHR::default()
            .semaphore(self.vk_semaphore)
            .handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD);
        let raw_fd = unsafe { ext.get_semaphore_fd(&info)? };
        super::optional_sync_fd_from_vk(raw_fd, "vkGetSemaphoreFdKHR(SYNC_FD)")
    }

    /// Mark this BO as "let process-exit clean up." Subsequent
    /// `Drop` is a no-op. Idempotent.
    /// **Only valid at final process exit** — see field doc.
    pub fn disarm(&mut self) {
        self.disarmed = true;
    }
}

impl Drop for ScanoutBo {
    fn drop(&mut self) {
        if self.disarmed {
            // Disarmed by shutdown-failed-disable path; let DRM-fd
            // close (process exit) reap GEM/FB and VkDevice teardown
            // releases the userspace handles. We are DELIBERATELY
            // leaking: this Drop deliberately skips vkDestroyImage,
            // vkFreeMemory, destroy_framebuffer, close_buffer(gem),
            // etc. — because touching them while KMS may still hold
            // the FB produces the `atomic remove_fb failed with -22`
            // warning that strands Wayland host sessions.
            log::warn!(
                "ScanoutBo disarmed (atomic disable_output failed); \
                 leaking FB/GEM/Vk to be reaped by DRM-fd close"
            );
            return;
        }
        // Defensive fence-fd cleanup. If the bo was Submitted /
        // Pending / OnScreen / Retiring at drop time (mid-flight
        // shutdown, or modeset that didn't go through the explicit
        // drain path), close any held fence fds so they don't leak.
        // Kernel-side sync_file refs survive our fd close until the
        // DRM device closes — atomic flip will still complete or
        // fail safely on its own.
        let released = self.state.transition_to_free_after_modeset_reset();
        if let Some(fd) = released.in_fence {
            // SAFETY: fd was inserted by transition_to_submitted; we
            // are the unique owner.
            drop(unsafe { OwnedFd::from_raw_fd(fd) });
        }
        if let Some(fd) = released.release_fence {
            drop(unsafe { OwnedFd::from_raw_fd(fd) });
        }

        // DRM-side teardown next: framebuffer references the GEM
        // handle; both must be released before we free the underlying
        // memory the dma-buf was exported from.
        if let Some(fb) = self.fb_handle.take()
            && let Err(e) = self.drm.destroy_framebuffer(fb)
        {
            log::warn!("drm destroy_framebuffer failed: {e}");
        }
        if let Some(h) = self.gem_handle.take()
            && let Err(e) = self.drm.close_buffer(h)
        {
            log::warn!("drm close_buffer (gem) failed: {e}");
        }

        unsafe {
            // Transfer resources (staging mapping must release before
            // memory is freed; command pool releases its CB).
            let t = std::mem::replace(
                &mut self.vk_transfer,
                TransferResources {
                    command_pool: vk::CommandPool::null(),
                    command_buffer: vk::CommandBuffer::null(),
                    staging_buffer: vk::Buffer::null(),
                    staging_memory: vk::DeviceMemory::null(),
                    staging_mapped: std::ptr::NonNull::dangling(),
                    staging_size: 0,
                },
            );
            if t.command_pool != vk::CommandPool::null() {
                self.vk.device.unmap_memory(t.staging_memory);
                self.vk.device.destroy_buffer(t.staging_buffer, None);
                self.vk.device.free_memory(t.staging_memory, None);
                self.vk.device.destroy_command_pool(t.command_pool, None);
            }

            // Image view before image, image before memory, then
            // semaphore.
            if self.vk_image_view != vk::ImageView::null() {
                self.vk.device.destroy_image_view(self.vk_image_view, None);
            }
            self.vk.device.destroy_image(self.vk_image, None);
            self.vk.device.free_memory(self.vk_memory, None);
            if self.vk_semaphore != vk::Semaphore::null() {
                self.vk.device.destroy_semaphore(self.vk_semaphore, None);
            }
        }
    }
}

/// Handle returned by [`ScanoutBoPool::register_alien`] — index into
/// `pool.bos` plus a generation token so a stale handle can't access
/// a re-used slot. Phase 4.2.4 design §3.3.2.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AlienBoHandle {
    pub index: u32,
}

impl ScanoutBoPool {
    /// Register a client-imported `DrawableImage` as an alien BO in
    /// the pool. The DrawableImage's underlying `VkDeviceMemory` is
    /// already allocated; we run the same `add_fb2` framebuffer
    /// registration the pool's owned BOs use, with the imported
    /// memory's GEM handle plus its DRM modifier.
    ///
    /// Phase 4.2.4 first-cut: returns `Err` because the
    /// VkDeviceMemory → GEM handle bridge is non-trivial and lives
    /// behind the live KMS Flip integration. The wire surface is in
    /// place so the dispatcher's choose_path Flip / DirectScanout
    /// branches plumb correctly; live registration arrives with the
    /// vng + Venus smoke for §5.5 hardware coverage.
    pub fn register_alien(
        &mut self,
        _drawable: &super::target::DrawableImage,
    ) -> io::Result<AlienBoHandle> {
        Err(io::Error::other(
            "ScanoutBoPool::register_alien: live KMS Flip integration not yet wired \
             (Phase 4.2.4 design §5.5 hardware coverage smoke)",
        ))
    }

    /// Drop a previously registered alien BO. Releases the framebuffer
    /// registration and removes the entry from `bos`. No-op if the
    /// handle's index is out of range.
    #[allow(dead_code)]
    pub fn unregister_alien(&mut self, _handle: AlienBoHandle) -> io::Result<()> {
        // Counterpart to register_alien — unimplemented for the same
        // reason. The plan's Task 29 test covers the round-trip once
        // both halves land.
        Ok(())
    }

    /// Reset every bo in the pool to `Free`, draining any in-flight
    /// fence fds. Used by the modeset / hot-config path (resize, mode
    /// change, hotplug — design §2 "Modeset / hot-config events").
    ///
    /// Order of operations:
    ///
    /// 1. `vkDeviceWaitIdle` on the device — heavy hammer that waits
    ///    for any in-flight `vkQueueSubmit2` work to complete. Cheap
    ///    in steady state (no work) and conservatively correct for
    ///    `Submitted`-phase bos which would otherwise have GPU work
    ///    racing the DRM tear-down.
    /// 2. For each bo, advance state machine to `Free` via
    ///    `transition_to_free_after_modeset_reset` and close any
    ///    returned fence fds.
    ///
    /// Pool dimensions stay the same; this is "reset state machine,
    /// keep the bos." Re-allocating bos with new dimensions is the
    /// caller's responsibility (drop the pool, allocate a fresh one
    /// with `ScanoutBoPool::allocate`).
    // Consumers: Drop, and `PlatformBackend::reset_scanout_bos_for_suspend`
    // (VT-switch suspend reclaims orphaned scanout BOs after master loss).
    pub fn drain_all_pending(&mut self, vk: &VkContext) {
        if let Err(e) = unsafe { vk.device.device_wait_idle() } {
            log::warn!("scanout pool drain: vkDeviceWaitIdle: {e}");
        }
        for bo in &mut self.bos {
            let released = bo.state.transition_to_free_after_modeset_reset();
            if let Some(fd) = released.in_fence {
                // SAFETY: fd inserted by transition_to_submitted; unique owner.
                drop(unsafe { OwnedFd::from_raw_fd(fd) });
            }
            if let Some(fd) = released.release_fence {
                drop(unsafe { OwnedFd::from_raw_fd(fd) });
            }
        }
    }

    /// True if any bo in this pool is in `BoPhase::Pending` —
    /// i.e. an atomic flip was accepted by KMS and the kernel
    /// hasn't yet emitted its pageflip-complete event for that
    /// flip. Used by the shutdown sequence to wait until KMS
    /// quiesces before issuing `disable_output`. Calling
    /// `disable_output` while a Pending bo exists is what
    /// produces the `atomic remove_fb failed with -22` kernel
    /// warning that leaves Wayland host compositors stranded.
    pub fn has_pending_pageflip(&self) -> bool {
        self.bos.iter().any(|b| b.state.phase == BoPhase::Pending)
    }

    /// Allocate `count` Vulkan-first bos for one output. Phase 4.1.2
    /// uses 3 bos per pool (design §2). On failure the partial pool
    /// is dropped (each successfully-allocated bo destroys its own
    /// resources via `ScanoutBo::Drop`).
    pub(crate) fn allocate(
        vk: Arc<VkContext>,
        drm: Rc<crate::drm::Device>,
        route: ScanoutRoute,
        render_physical_device: Option<vk::PhysicalDevice>,
        width: u32,
        height: u32,
        count: usize,
        scanout_modifiers: &[u64],
    ) -> io::Result<Self> {
        let compatibility = probe_dmabuf_scanout_compatibility(
            &vk,
            &drm,
            route,
            render_physical_device,
            scanout_modifiers,
        );
        if route.is_cross_device() && compatibility.verdict.blocks_attempt() {
            let DmabufScanoutVerdict::Incompatible(reason) = &compatibility.verdict else {
                unreachable!("only an incompatible verdict blocks allocation")
            };
            return Err(io::Error::other(format!(
                "dma-buf route {} -> {} is incompatible: {reason:?} \
                 (modifiers={}, linear={:?})",
                route.render_device_key,
                route.kms_device_key,
                format_modifiers(&compatibility.modifiers),
                compatibility.linear,
            )));
        }

        let plans = allocation_plans_for_route(vk.image_drm_format_modifier, &compatibility);
        let mut bos = Vec::with_capacity(count);
        for _ in 0..count {
            bos.push(ScanoutBo::allocate(
                Arc::clone(&vk),
                Rc::clone(&drm),
                width,
                height,
                &plans,
            )?);
        }
        Ok(Self {
            bos,
            width,
            height,
            route,
            compatibility,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ScanoutAllocationPlan {
    /// A single DRM modifier from the KMS/Vulkan intersection.
    DrmModifier(u64),
    /// Linear VkImage, but register the DRM framebuffer with an
    /// explicit DRM_FORMAT_MOD_LINEAR modifier.
    ExplicitLinear,
    /// Historical fallback: linear VkImage, untagged addfb2.
    LegacyLinear,
}

impl ScanoutAllocationPlan {
    fn describe(self) -> String {
        match self {
            Self::DrmModifier(modifier) => format!("modifier=0x{modifier:x}"),
            Self::ExplicitLinear => "explicit-linear".to_string(),
            Self::LegacyLinear => "legacy-linear".to_string(),
        }
    }
}

fn scanout_allocation_plans(
    image_drm_format_modifier: bool,
    modifier_candidates: &[u64],
) -> Vec<ScanoutAllocationPlan> {
    let mut plans = Vec::new();
    if image_drm_format_modifier {
        plans.extend(
            modifier_candidates
                .iter()
                .copied()
                .map(ScanoutAllocationPlan::DrmModifier),
        );
    }
    plans.push(ScanoutAllocationPlan::ExplicitLinear);
    plans.push(ScanoutAllocationPlan::LegacyLinear);
    plans
}

fn allocation_plans_for_route(
    image_drm_format_modifier: bool,
    compatibility: &DmabufScanoutCompatibility,
) -> Vec<ScanoutAllocationPlan> {
    if !compatibility.route.is_cross_device() {
        return scanout_allocation_plans(image_drm_format_modifier, &compatibility.modifiers);
    }

    if matches!(&compatibility.verdict, DmabufScanoutVerdict::Unknown(_)) {
        // Preserve the pre-probe behavior when metadata is incomplete: try
        // every advertised modifier followed by both linear registrations and
        // let PRIME_FD_TO_HANDLE/addfb2 provide the authoritative answer.
        return scanout_allocation_plans(image_drm_format_modifier, &compatibility.modifiers);
    }

    let mut plans = Vec::new();
    if image_drm_format_modifier {
        plans.extend(
            compatibility
                .modifiers
                .iter()
                .copied()
                .map(ScanoutAllocationPlan::DrmModifier),
        );
    }
    match compatibility.linear {
        LinearScanoutSupport::None => {}
        LinearScanoutSupport::ExplicitModifier => {
            plans.push(ScanoutAllocationPlan::ExplicitLinear);
        }
        LinearScanoutSupport::LegacyAddfb => {
            plans.push(ScanoutAllocationPlan::LegacyLinear);
        }
    }
    plans
}

const DRM_PRIME_CAP_IMPORT: u64 = 1 << 0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RenderDeviceMatch {
    Match,
    Mismatch,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrimeImportSupport {
    Supported,
    Unsupported,
    Unknown,
}

/// Probe the advertised dma-buf path from one Vulkan renderer to one KMS
/// device without allocating, exporting, or importing a buffer.
///
/// The result combines Vulkan external-memory image properties, the KMS
/// primary plane's `IN_FORMATS` list, and the sink DRM fd's
/// `DRM_CAP_PRIME` import bit. It deliberately remains a necessary-condition
/// probe: only a later real allocation can prove that the two drivers accept
/// a particular dma-buf instance and pitch.
pub(crate) fn probe_dmabuf_scanout_compatibility(
    vk: &VkContext,
    drm: &crate::drm::Device,
    route: ScanoutRoute,
    render_physical_device: Option<vk::PhysicalDevice>,
    kms_scanout_modifiers: &[u64],
) -> DmabufScanoutCompatibility {
    let render_device_match = match render_physical_device {
        Some(physical_device) if physical_device == vk.physical_device => RenderDeviceMatch::Match,
        Some(_) => RenderDeviceMatch::Mismatch,
        None => RenderDeviceMatch::Unknown,
    };
    let prime_import = match drm.get_driver_capability(DriverCapability::Prime) {
        Ok(capabilities) if capabilities & DRM_PRIME_CAP_IMPORT != 0 => {
            PrimeImportSupport::Supported
        }
        Ok(_) => PrimeImportSupport::Unsupported,
        Err(err) => {
            log::warn!(
                "dma-buf probe {} -> {}: DRM_CAP_PRIME query failed: {err}",
                route.render_device_key,
                route.kms_device_key,
            );
            PrimeImportSupport::Unknown
        }
    };

    let external_memory_fd_available = vk.external_memory_fd.is_some();
    let modifiers = if external_memory_fd_available {
        scanout_modifier_candidates(vk, kms_scanout_modifiers)
    } else {
        Vec::new()
    };
    let linear_exportable = external_memory_fd_available && scanout_linear_is_exportable(vk);
    let compatibility = classify_dmabuf_scanout_compatibility(
        route,
        external_memory_fd_available,
        render_device_match,
        prime_import,
        modifiers,
        linear_exportable,
        kms_scanout_modifiers,
    );

    log::info!(
        "dma-buf probe {} -> {}: modifiers={} linear={:?} verdict={:?}",
        route.render_device_key,
        route.kms_device_key,
        format_modifiers(&compatibility.modifiers),
        compatibility.linear,
        compatibility.verdict,
    );
    compatibility
}

fn classify_dmabuf_scanout_compatibility(
    route: ScanoutRoute,
    external_memory_fd_available: bool,
    render_device_match: RenderDeviceMatch,
    prime_import: PrimeImportSupport,
    modifiers: Vec<u64>,
    linear_exportable: bool,
    kms_scanout_modifiers: &[u64],
) -> DmabufScanoutCompatibility {
    let linear = if !linear_exportable {
        LinearScanoutSupport::None
    } else if kms_scanout_modifiers.is_empty() {
        LinearScanoutSupport::LegacyAddfb
    } else if kms_scanout_modifiers.contains(&super::dri3::DRM_FORMAT_MOD_LINEAR) {
        LinearScanoutSupport::ExplicitModifier
    } else {
        LinearScanoutSupport::None
    };
    let verdict = if !external_memory_fd_available {
        DmabufScanoutVerdict::Incompatible(
            DmabufScanoutIncompatibility::VulkanExternalMemoryFdUnavailable,
        )
    } else if render_device_match == RenderDeviceMatch::Mismatch {
        DmabufScanoutVerdict::Incompatible(DmabufScanoutIncompatibility::RenderDeviceMismatch)
    } else if prime_import == PrimeImportSupport::Unsupported {
        DmabufScanoutVerdict::Incompatible(DmabufScanoutIncompatibility::KmsPrimeImportUnsupported)
    } else {
        let mut uncertainty = Vec::new();
        if render_device_match == RenderDeviceMatch::Unknown {
            uncertainty.push(DmabufScanoutUncertainty::RenderDeviceIdentityUnavailable);
        }
        if prime_import == PrimeImportSupport::Unknown {
            uncertainty.push(DmabufScanoutUncertainty::KmsPrimeImportCapabilityUnavailable);
        }
        if modifiers.is_empty() && linear == LinearScanoutSupport::None {
            uncertainty.push(DmabufScanoutUncertainty::NoAdvertisedSharedLayout);
        }
        if uncertainty.is_empty() {
            DmabufScanoutVerdict::Compatible
        } else {
            DmabufScanoutVerdict::Unknown(uncertainty)
        }
    };
    DmabufScanoutCompatibility {
        route,
        modifiers,
        linear,
        verdict,
    }
}

fn scanout_linear_is_exportable(vk: &VkContext) -> bool {
    let mut external_info = vk::PhysicalDeviceExternalImageFormatInfo::default()
        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let format_info = vk::PhysicalDeviceImageFormatInfo2::default()
        .format(vk::Format::B8G8R8A8_UNORM)
        .ty(vk::ImageType::TYPE_2D)
        .tiling(vk::ImageTiling::LINEAR)
        .usage(scanout_image_usage())
        .push_next(&mut external_info);

    let mut external_props = vk::ExternalImageFormatProperties::default();
    let mut props = vk::ImageFormatProperties2::default().push_next(&mut external_props);
    if unsafe {
        vk.instance.get_physical_device_image_format_properties2(
            vk.physical_device,
            &format_info,
            &mut props,
        )
    }
    .is_err()
    {
        return false;
    }

    let memory = external_props.external_memory_properties;
    memory
        .external_memory_features
        .contains(vk::ExternalMemoryFeatureFlags::EXPORTABLE)
        && memory
            .compatible_handle_types
            .contains(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
}

/// Whether scanout BO allocation should try `LINEAR` before the tiled
/// DRM modifiers (see [`order_scanout_modifier_candidates`]).
///
/// Driver-split policy, each entry HW-confirmed against a real dithered/
/// corrupted scanout:
/// - **NVIDIA proprietary** (GTX 1050/Pascal): the BLOCK_LINEAR_2D modifier
///   path produces a dithered display (issue from project notes).
/// - **Intel Mesa (ANV)** (Kaby Lake i5-7200U): the I915 Y_TILED modifier
///   (`0x0100000000000002`) selected first produces the same dithering.
///
/// AMD (RADV) is deliberately NOT in this set: RDNA4/gfx12 *requires* the
/// tiled modifier — a LINEAR scanout buffer corrupts there (issue #48) —
/// and RDNA2 scans out tiled fine. Other drivers default to tiled-first;
/// add them here only after a confirmed dithering report, not speculatively
/// (e.g. Asahi/M1 has not shown the problem and stays tiled-first).
fn scanout_prefers_linear(driver_id: vk::DriverId) -> bool {
    matches!(
        driver_id,
        vk::DriverId::NVIDIA_PROPRIETARY | vk::DriverId::INTEL_OPEN_SOURCE_MESA
    )
}

fn scanout_modifier_candidates(vk: &VkContext, kms_scanout_modifiers: &[u64]) -> Vec<u64> {
    if kms_scanout_modifiers.is_empty() {
        return Vec::new();
    }

    let vulkan = super::dri3::supported_modifiers(vk, vk::Format::B8G8R8A8_UNORM);
    let prefer_linear = scanout_prefers_linear(vk.driver_id);
    let candidates = order_scanout_modifier_candidates(
        kms_scanout_modifiers,
        &vulkan,
        prefer_linear,
        |modifier| scanout_modifier_is_single_plane_exportable(vk, modifier),
    );
    // Diagnostic for scanout-corruption reports (issue #48): show what
    // the plane offered vs. what survived the Vulkan/exportable filter,
    // so a card that simply has no tiled scanout modifier on offer is
    // distinguishable from one whose tiled modifier we rejected.
    log::info!(
        "scanout modifier select: kms_plane={} vulkan_supports={} -> candidates={}",
        format_modifiers(kms_scanout_modifiers),
        format_modifiers(&vulkan),
        format_modifiers(&candidates),
    );
    candidates
}

fn format_modifiers(modifiers: &[u64]) -> String {
    if modifiers.is_empty() {
        return "[]".to_string();
    }
    let joined = modifiers
        .iter()
        .map(|m| format!("0x{m:x}"))
        .collect::<Vec<_>>()
        .join(",");
    format!("[{joined}]")
}

/// Order the KMS/Vulkan modifier intersection into the sequence the
/// allocator tries.
///
/// Default (`prefer_linear = false`): **tiled modifiers first, `LINEAR` last.**
/// This fixes corruption on RDNA4/gfx12 (RX 9070 XT, issue #48) where a
/// Vulkan-rendered linear scanout buffer produces horizontal tiling
/// artifacts. RADV on gfx8/Polaris doesn't expose
/// `VK_EXT_image_drm_format_modifier`, so those cards never reach this
/// path and allocation falls through to the untagged-linear plan.
///
/// `prefer_linear = true`: **LINEAR first, tiled as fallback.**
/// Used for `NVIDIA_PROPRIETARY` where the BLOCK_LINEAR_2D modifier path
/// produces a dithered/scrambled display on Pascal hardware (GTX 1050,
/// GP107) even though allocation and KMS import succeed. This mirrors what
/// GBM does when passed the full modifier set: it implicitly selects LINEAR
/// for scanout on those cards.
///
/// Pure (no Vulkan calls of its own) so the ordering policy is unit
/// testable; `is_exportable` is the per-modifier single-plane check.
fn order_scanout_modifier_candidates(
    kms_scanout_modifiers: &[u64],
    vulkan_supported: &[u64],
    prefer_linear: bool,
    is_exportable: impl Fn(u64) -> bool,
) -> Vec<u64> {
    let mut candidates = Vec::new();

    // When LINEAR is preferred (NVIDIA), add it first if both sides advertise it.
    if prefer_linear
        && kms_scanout_modifiers.contains(&super::dri3::DRM_FORMAT_MOD_LINEAR)
        && vulkan_supported.contains(&super::dri3::DRM_FORMAT_MOD_LINEAR)
        && is_exportable(super::dri3::DRM_FORMAT_MOD_LINEAR)
    {
        candidates.push(super::dri3::DRM_FORMAT_MOD_LINEAR);
    }

    // Non-LINEAR modifiers in KMS-advertised order.
    for &modifier in kms_scanout_modifiers {
        if modifier == super::dri3::DRM_FORMAT_MOD_LINEAR {
            continue;
        }
        if vulkan_supported.contains(&modifier)
            && is_exportable(modifier)
            && !candidates.contains(&modifier)
        {
            candidates.push(modifier);
        }
    }

    // When tiled is preferred (default), LINEAR comes last.
    if !prefer_linear
        && kms_scanout_modifiers.contains(&super::dri3::DRM_FORMAT_MOD_LINEAR)
        && vulkan_supported.contains(&super::dri3::DRM_FORMAT_MOD_LINEAR)
        && is_exportable(super::dri3::DRM_FORMAT_MOD_LINEAR)
        && !candidates.contains(&super::dri3::DRM_FORMAT_MOD_LINEAR)
    {
        candidates.push(super::dri3::DRM_FORMAT_MOD_LINEAR);
    }

    candidates
}

fn scanout_modifier_is_single_plane_exportable(vk: &VkContext, modifier: u64) -> bool {
    use std::ffi::c_void;

    let mut modifier_info = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default()
        .drm_format_modifier(modifier)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    let mut external_info = vk::PhysicalDeviceExternalImageFormatInfo::default()
        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    external_info.p_next = std::ptr::from_mut(&mut modifier_info).cast::<c_void>();

    let mut format_info = vk::PhysicalDeviceImageFormatInfo2::default()
        .format(vk::Format::B8G8R8A8_UNORM)
        .ty(vk::ImageType::TYPE_2D)
        .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
        .usage(scanout_image_usage());
    format_info.p_next = std::ptr::from_mut(&mut external_info).cast::<c_void>();

    let mut external_props = vk::ExternalImageFormatProperties::default();
    let mut props2 = vk::ImageFormatProperties2::default().push_next(&mut external_props);
    if unsafe {
        vk.instance.get_physical_device_image_format_properties2(
            vk.physical_device,
            &format_info,
            &mut props2,
        )
    }
    .is_err()
    {
        return false;
    }

    external_props
        .external_memory_properties
        .external_memory_features
        .contains(vk::ExternalMemoryFeatureFlags::EXPORTABLE)
        && external_props
            .external_memory_properties
            .compatible_handle_types
            .contains(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
        && drm_modifier_plane_count(vk, modifier) == Some(1)
}

fn drm_modifier_plane_count(vk: &VkContext, modifier: u64) -> Option<u32> {
    let modifier_count = {
        let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
        let mut format_props = vk::FormatProperties2::default().push_next(&mut list);
        unsafe {
            vk.instance.get_physical_device_format_properties2(
                vk.physical_device,
                vk::Format::B8G8R8A8_UNORM,
                &mut format_props,
            );
        }
        list.drm_format_modifier_count
    };
    if modifier_count == 0 {
        return None;
    }

    let mut props_storage =
        vec![vk::DrmFormatModifierPropertiesEXT::default(); modifier_count as usize];
    let mut list = vk::DrmFormatModifierPropertiesListEXT::default()
        .drm_format_modifier_properties(&mut props_storage);
    let mut format_props = vk::FormatProperties2::default().push_next(&mut list);
    unsafe {
        vk.instance.get_physical_device_format_properties2(
            vk.physical_device,
            vk::Format::B8G8R8A8_UNORM,
            &mut format_props,
        );
    }
    let entries = list.drm_format_modifier_count as usize;
    props_storage
        .iter()
        .take(entries)
        .find(|p| p.drm_format_modifier == modifier)
        .map(|p| p.drm_format_modifier_plane_count)
}

fn scanout_image_usage() -> vk::ImageUsageFlags {
    vk::ImageUsageFlags::COLOR_ATTACHMENT
        | vk::ImageUsageFlags::TRANSFER_DST
        | vk::ImageUsageFlags::SAMPLED
}

fn addfb_flags_for_modifier(modifier: Option<u64>) -> FbCmd2Flags {
    if modifier.is_some() {
        FbCmd2Flags::MODIFIERS
    } else {
        FbCmd2Flags::empty()
    }
}

fn destroy_scanout_image(vk: &VkContext, image: vk::Image, memory: vk::DeviceMemory) {
    unsafe {
        vk.device.destroy_image(image, None);
        vk.device.free_memory(memory, None);
    }
}

/// Outputs of [`allocate_vk_scanout_image`]: a freshly-bound VkImage,
/// its memory, the dma-buf fd we exported from that memory, the
/// row pitch the driver chose, and the optional DRM modifier to use
/// for framebuffer registration.
struct VkScanoutImage {
    image: vk::Image,
    memory: vk::DeviceMemory,
    dmabuf: OwnedFd,
    pitch: u32,
    modifier: Option<u64>,
}

/// Allocate a scanout `VkImage` whose memory is dma-buf-exportable;
/// bind memory; export the dma-buf; query the row pitch the driver
/// picked.
fn allocate_vk_scanout_image(
    vk: &VkContext,
    width: u32,
    height: u32,
    plan: ScanoutAllocationPlan,
) -> Result<VkScanoutImage, vk::Result> {
    let ext_memory_fd = vk
        .external_memory_fd
        .as_ref()
        .ok_or(vk::Result::ERROR_EXTENSION_NOT_PRESENT)?;

    let drm_modifier = match plan {
        ScanoutAllocationPlan::DrmModifier(modifier) => Some(modifier),
        ScanoutAllocationPlan::ExplicitLinear | ScanoutAllocationPlan::LegacyLinear => None,
    };

    let mut external_info = vk::ExternalMemoryImageCreateInfo::default()
        .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let modifier_storage = [drm_modifier.unwrap_or(super::dri3::DRM_FORMAT_MOD_LINEAR)];
    let mut modifier_list = vk::ImageDrmFormatModifierListCreateInfoEXT::default()
        .drm_format_modifiers(if drm_modifier.is_some() {
            &modifier_storage
        } else {
            &[]
        });

    let tiling = match plan {
        ScanoutAllocationPlan::DrmModifier(_) => vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT,
        ScanoutAllocationPlan::ExplicitLinear | ScanoutAllocationPlan::LegacyLinear => {
            vk::ImageTiling::LINEAR
        }
    };

    let image_info_base = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(vk::Format::B8G8R8A8_UNORM)
        .extent(vk::Extent3D {
            width,
            height,
            depth: 1,
        })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(tiling)
        .usage(scanout_image_usage())
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED);

    let image_info = if drm_modifier.is_some() {
        image_info_base
            .push_next(&mut external_info)
            .push_next(&mut modifier_list)
    } else {
        image_info_base.push_next(&mut external_info)
    };

    let image = unsafe { vk.device.create_image(&image_info, None)? };

    // 3. Memory: dma-buf-exportable + dedicated to this image.
    let mem_reqs = unsafe { vk.device.get_image_memory_requirements(image) };
    let mem_props = unsafe {
        vk.instance
            .get_physical_device_memory_properties(vk.physical_device)
    };
    let memory_type_index = match pick_memory_type(
        &mem_props,
        mem_reqs.memory_type_bits,
        vk::MemoryPropertyFlags::DEVICE_LOCAL,
    )
    .or_else(|| {
        pick_memory_type(
            &mem_props,
            mem_reqs.memory_type_bits,
            vk::MemoryPropertyFlags::empty(),
        )
    }) {
        Some(i) => i,
        None => {
            unsafe { vk.device.destroy_image(image, None) };
            return Err(vk::Result::ERROR_OUT_OF_DEVICE_MEMORY);
        }
    };

    let mut export_info = vk::ExportMemoryAllocateInfo::default()
        .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
    let alloc_info = vk::MemoryAllocateInfo::default()
        .allocation_size(mem_reqs.size)
        .memory_type_index(memory_type_index)
        .push_next(&mut export_info)
        .push_next(&mut dedicated);

    let memory = match unsafe { vk.device.allocate_memory(&alloc_info, None) } {
        Ok(m) => m,
        Err(e) => {
            unsafe { vk.device.destroy_image(image, None) };
            return Err(e);
        }
    };

    if let Err(e) = unsafe { vk.device.bind_image_memory(image, memory, 0) } {
        unsafe {
            vk.device.free_memory(memory, None);
            vk.device.destroy_image(image, None);
        }
        return Err(e);
    }

    let selected_modifier = match plan {
        ScanoutAllocationPlan::DrmModifier(_) => {
            let Some(ext) = vk.image_drm_format_modifier_ext.as_ref() else {
                unsafe {
                    vk.device.free_memory(memory, None);
                    vk.device.destroy_image(image, None);
                }
                return Err(vk::Result::ERROR_EXTENSION_NOT_PRESENT);
            };
            let mut props = vk::ImageDrmFormatModifierPropertiesEXT::default();
            if let Err(e) =
                unsafe { ext.get_image_drm_format_modifier_properties(image, &mut props) }
            {
                unsafe {
                    vk.device.free_memory(memory, None);
                    vk.device.destroy_image(image, None);
                }
                return Err(e);
            }
            Some(props.drm_format_modifier)
        }
        ScanoutAllocationPlan::ExplicitLinear => Some(super::dri3::DRM_FORMAT_MOD_LINEAR),
        ScanoutAllocationPlan::LegacyLinear => None,
    };

    // Row pitch from the driver. We need this for KMS addfb2.
    // Modifier-tiled images MUST be queried with a MEMORY_PLANE aspect;
    // COLOR is a validation error (the single-plane scanout buffer is
    // plane 0). LINEAR-tiled fallbacks keep the COLOR aspect.
    let layout_aspect = match plan {
        ScanoutAllocationPlan::DrmModifier(_) => vk::ImageAspectFlags::MEMORY_PLANE_0_EXT,
        ScanoutAllocationPlan::ExplicitLinear | ScanoutAllocationPlan::LegacyLinear => {
            vk::ImageAspectFlags::COLOR
        }
    };
    let layout = unsafe {
        vk.device.get_image_subresource_layout(
            image,
            vk::ImageSubresource {
                aspect_mask: layout_aspect,
                mip_level: 0,
                array_layer: 0,
            },
        )
    };
    let pitch = u32::try_from(layout.row_pitch).unwrap_or(u32::MAX);

    // 4. Export the bound memory as a dma-buf fd.
    let get_fd_info = vk::MemoryGetFdInfoKHR::default()
        .memory(memory)
        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let raw_fd = match unsafe { ext_memory_fd.get_memory_fd(&get_fd_info) } {
        Ok(fd) => fd,
        Err(e) => {
            unsafe {
                vk.device.free_memory(memory, None);
                vk.device.destroy_image(image, None);
            }
            return Err(e);
        }
    };
    let dmabuf = super::owned_fd_from_vk(raw_fd, "vkGetMemoryFdKHR(DMA_BUF)")?;

    Ok(VkScanoutImage {
        image,
        memory,
        dmabuf,
        pitch,
        modifier: selected_modifier,
    })
}

/// Adapter that lets a freshly-imported GEM handle be passed to
/// drm 0.15's `add_planar_framebuffer` as a `PlanarBuffer`. Single
/// plane; modifier is present for explicit-modifier addfb2 paths and
/// absent only for the legacy untagged-linear fallback.
struct VkScanoutFb {
    gem_handle: DrmBufferHandle,
    width: u32,
    height: u32,
    pitch: u32,
    modifier: Option<u64>,
}

impl DrmPlanarBuffer for VkScanoutFb {
    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
    fn format(&self) -> DrmFourcc {
        DrmFourcc::Xrgb8888
    }
    fn modifier(&self) -> Option<DrmModifier> {
        self.modifier.map(DrmModifier::from)
    }
    fn pitches(&self) -> [u32; 4] {
        [self.pitch, 0, 0, 0]
    }
    fn handles(&self) -> [Option<DrmBufferHandle>; 4] {
        [Some(self.gem_handle), None, None, None]
    }
    fn offsets(&self) -> [u32; 4] {
        [0, 0, 0, 0]
    }
}

/// Create a binary `VkSemaphore` whose payload can be exported as a
/// SYNC_FD via `vkGetSemaphoreFdKHR`. Reused for the bo's full
/// lifetime; the fd payload churns per submit.
fn create_export_semaphore(vk: &VkContext) -> Result<vk::Semaphore, vk::Result> {
    let mut export_info = vk::ExportSemaphoreCreateInfo::default()
        .handle_types(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD);
    let create_info = vk::SemaphoreCreateInfo::default().push_next(&mut export_info);
    unsafe { vk.device.create_semaphore(&create_info, None) }
}

/// Allocate per-bo transfer resources: command pool + 1 command
/// buffer; staging buffer + host-mapped device memory sized for one
/// XRGB8888 frame at (width × height).
fn allocate_transfer_resources(
    vk: &VkContext,
    width: u32,
    height: u32,
) -> Result<TransferResources, vk::Result> {
    let pool_info = vk::CommandPoolCreateInfo::default()
        .queue_family_index(vk.graphics_queue_family)
        .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
    let command_pool = unsafe { vk.device.create_command_pool(&pool_info, None)? };

    let cb_info = vk::CommandBufferAllocateInfo::default()
        .command_pool(command_pool)
        .level(vk::CommandBufferLevel::PRIMARY)
        .command_buffer_count(1);
    let command_buffers = match unsafe { vk.device.allocate_command_buffers(&cb_info) } {
        Ok(cbs) => cbs,
        Err(e) => {
            unsafe { vk.device.destroy_command_pool(command_pool, None) };
            return Err(e);
        }
    };
    let command_buffer = command_buffers[0];

    let staging_size: u64 = u64::from(width) * u64::from(height) * 4;
    let buf_info = vk::BufferCreateInfo::default()
        .size(staging_size)
        .usage(vk::BufferUsageFlags::TRANSFER_SRC)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    let staging_buffer = match unsafe { vk.device.create_buffer(&buf_info, None) } {
        Ok(b) => b,
        Err(e) => {
            unsafe { vk.device.destroy_command_pool(command_pool, None) };
            return Err(e);
        }
    };

    let mem_reqs = unsafe { vk.device.get_buffer_memory_requirements(staging_buffer) };
    let mem_props = unsafe {
        vk.instance
            .get_physical_device_memory_properties(vk.physical_device)
    };
    let want_strict = vk::MemoryPropertyFlags::HOST_VISIBLE
        | vk::MemoryPropertyFlags::HOST_COHERENT
        | vk::MemoryPropertyFlags::DEVICE_LOCAL;
    let want_loose = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
    let memory_type_index = pick_memory_type(&mem_props, mem_reqs.memory_type_bits, want_strict)
        .or_else(|| pick_memory_type(&mem_props, mem_reqs.memory_type_bits, want_loose))
        .ok_or(vk::Result::ERROR_OUT_OF_DEVICE_MEMORY);
    let memory_type_index = match memory_type_index {
        Ok(i) => i,
        Err(e) => {
            unsafe {
                vk.device.destroy_buffer(staging_buffer, None);
                vk.device.destroy_command_pool(command_pool, None);
            }
            return Err(e);
        }
    };

    let alloc_info = vk::MemoryAllocateInfo::default()
        .allocation_size(mem_reqs.size)
        .memory_type_index(memory_type_index);
    let staging_memory = match unsafe { vk.device.allocate_memory(&alloc_info, None) } {
        Ok(m) => m,
        Err(e) => {
            unsafe {
                vk.device.destroy_buffer(staging_buffer, None);
                vk.device.destroy_command_pool(command_pool, None);
            }
            return Err(e);
        }
    };
    if let Err(e) = unsafe {
        vk.device
            .bind_buffer_memory(staging_buffer, staging_memory, 0)
    } {
        unsafe {
            vk.device.free_memory(staging_memory, None);
            vk.device.destroy_buffer(staging_buffer, None);
            vk.device.destroy_command_pool(command_pool, None);
        }
        return Err(e);
    }

    let mapped_ptr = match unsafe {
        vk.device
            .map_memory(staging_memory, 0, staging_size, vk::MemoryMapFlags::empty())
    } {
        Ok(p) => p,
        Err(e) => {
            unsafe {
                vk.device.free_memory(staging_memory, None);
                vk.device.destroy_buffer(staging_buffer, None);
                vk.device.destroy_command_pool(command_pool, None);
            }
            return Err(e);
        }
    };
    let staging_mapped =
        std::ptr::NonNull::new(mapped_ptr.cast::<u8>()).expect("vkMapMemory returned non-null");

    Ok(TransferResources {
        command_pool,
        command_buffer,
        staging_buffer,
        staging_memory,
        staging_mapped,
        staging_size,
    })
}

fn pick_memory_type(
    props: &vk::PhysicalDeviceMemoryProperties,
    type_bits: u32,
    required: vk::MemoryPropertyFlags,
) -> Option<u32> {
    (0..props.memory_type_count).find(|&i| {
        let candidate = type_bits & (1 << i) != 0;
        candidate
            && props.memory_types[i as usize]
                .property_flags
                .contains(required)
    })
}

// Compile-only check that `export_signaled_fd`'s call into
// `external_semaphore_fd::Device::get_semaphore_fd` keeps the same
// argument shape. If ash bumps and the signature changes, this
// function fails to compile and breaks the build before any
// integration test runs.
#[cfg(test)]
#[allow(dead_code)]
fn _compile_check_export_signature(
    ext: &ash::khr::external_semaphore_fd::Device,
    semaphore: vk::Semaphore,
) {
    let info = vk::SemaphoreGetFdInfoKHR::default()
        .semaphore(semaphore)
        .handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD);
    let _: Result<i32, vk::Result> = unsafe { ext.get_semaphore_fd(&info) };
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINEAR: u64 = super::super::dri3::DRM_FORMAT_MOD_LINEAR;
    // Representative tiled modifiers (real AMD GFX9+ vendor-tiled values).
    const TILED_A: u64 = 0x0200_0000_0000_0008;
    const TILED_B: u64 = 0x0200_0000_0000_000a;

    #[test]
    fn modifier_order_prefers_tiled_over_linear() {
        // KMS advertises linear first, then a tiled modifier; both are
        // Vulkan-supported and exportable. Tiled must win — issue #48.
        let candidates = order_scanout_modifier_candidates(
            &[LINEAR, TILED_A],
            &[LINEAR, TILED_A],
            false,
            |_| true,
        );
        assert_eq!(
            candidates,
            vec![TILED_A, LINEAR],
            "tiled modifier must precede LINEAR"
        );
    }

    #[test]
    fn modifier_order_keeps_kms_order_among_tiled() {
        let candidates = order_scanout_modifier_candidates(
            &[TILED_B, TILED_A, LINEAR],
            &[TILED_A, TILED_B, LINEAR],
            false,
            |_| true,
        );
        assert_eq!(candidates, vec![TILED_B, TILED_A, LINEAR]);
    }

    #[test]
    fn modifier_order_drops_tiled_not_supported_by_vulkan() {
        let candidates =
            order_scanout_modifier_candidates(&[TILED_A, LINEAR], &[LINEAR], false, |_| true);
        assert_eq!(candidates, vec![LINEAR], "TILED_A not in the Vulkan set");
    }

    #[test]
    fn modifier_order_drops_non_exportable_tiled() {
        // A multi-plane (DCC) tiled modifier fails the single-plane
        // exportability check and must be skipped, leaving LINEAR.
        let candidates =
            order_scanout_modifier_candidates(&[TILED_A, LINEAR], &[TILED_A, LINEAR], false, |m| {
                m != TILED_A
            });
        assert_eq!(candidates, vec![LINEAR]);
    }

    #[test]
    fn modifier_order_linear_only_plane_yields_linear() {
        let candidates = order_scanout_modifier_candidates(&[LINEAR], &[LINEAR], false, |_| true);
        assert_eq!(candidates, vec![LINEAR]);
    }

    #[test]
    fn modifier_order_empty_when_no_intersection() {
        let candidates = order_scanout_modifier_candidates(&[TILED_A], &[TILED_B], false, |_| true);
        assert!(candidates.is_empty());
    }

    #[test]
    fn modifier_order_rejects_non_exportable_linear() {
        let candidates = order_scanout_modifier_candidates(&[LINEAR], &[LINEAR], false, |_| false);
        assert!(candidates.is_empty());
    }

    fn test_route(cross_device: bool) -> ScanoutRoute {
        let render_device_key = crate::platform::drm::DrmDeviceKey {
            major: 226,
            minor: 0,
        };
        let kms_device_key = if cross_device {
            crate::platform::drm::DrmDeviceKey {
                major: 226,
                minor: 1,
            }
        } else {
            render_device_key
        };
        ScanoutRoute::new(render_device_key, kms_device_key)
    }

    #[test]
    fn compatibility_accepts_shared_modifier_with_prime_import() {
        let compatibility = classify_dmabuf_scanout_compatibility(
            test_route(true),
            true,
            RenderDeviceMatch::Match,
            PrimeImportSupport::Supported,
            vec![TILED_A],
            false,
            &[TILED_A],
        );
        assert!(compatibility.route.is_cross_device());
        assert_eq!(compatibility.verdict, DmabufScanoutVerdict::Compatible);
        assert!(!compatibility.verdict.blocks_attempt());
        assert_eq!(compatibility.modifiers, vec![TILED_A]);
        assert_eq!(compatibility.linear, LinearScanoutSupport::None);
    }

    #[test]
    fn compatibility_accepts_explicit_linear_path() {
        let compatibility = classify_dmabuf_scanout_compatibility(
            test_route(true),
            true,
            RenderDeviceMatch::Match,
            PrimeImportSupport::Supported,
            Vec::new(),
            true,
            &[LINEAR],
        );
        assert_eq!(compatibility.verdict, DmabufScanoutVerdict::Compatible);
        assert_eq!(compatibility.linear, LinearScanoutSupport::ExplicitModifier);
    }

    #[test]
    fn compatibility_records_legacy_linear_when_in_formats_is_absent() {
        let compatibility = classify_dmabuf_scanout_compatibility(
            test_route(true),
            true,
            RenderDeviceMatch::Match,
            PrimeImportSupport::Supported,
            Vec::new(),
            true,
            &[],
        );
        assert_eq!(compatibility.verdict, DmabufScanoutVerdict::Compatible);
        assert_eq!(compatibility.linear, LinearScanoutSupport::LegacyAddfb);
    }

    #[test]
    fn compatibility_requires_sink_prime_import() {
        let compatibility = classify_dmabuf_scanout_compatibility(
            test_route(true),
            true,
            RenderDeviceMatch::Match,
            PrimeImportSupport::Unsupported,
            vec![TILED_A],
            true,
            &[TILED_A, LINEAR],
        );
        assert_eq!(
            compatibility.verdict,
            DmabufScanoutVerdict::Incompatible(
                DmabufScanoutIncompatibility::KmsPrimeImportUnsupported
            )
        );
        assert!(compatibility.verdict.blocks_attempt());
    }

    #[test]
    fn compatibility_keeps_missing_vulkan_identity_unknown_and_attemptable() {
        let compatibility = classify_dmabuf_scanout_compatibility(
            test_route(true),
            true,
            RenderDeviceMatch::Unknown,
            PrimeImportSupport::Supported,
            vec![TILED_A],
            true,
            &[TILED_A, LINEAR],
        );
        assert_eq!(
            compatibility.verdict,
            DmabufScanoutVerdict::Unknown(vec![
                DmabufScanoutUncertainty::RenderDeviceIdentityUnavailable
            ])
        );
        assert!(!compatibility.verdict.blocks_attempt());
    }

    #[test]
    fn compatibility_rejects_verified_vulkan_device_mismatch() {
        let compatibility = classify_dmabuf_scanout_compatibility(
            test_route(true),
            true,
            RenderDeviceMatch::Mismatch,
            PrimeImportSupport::Supported,
            vec![TILED_A],
            true,
            &[TILED_A, LINEAR],
        );
        assert_eq!(
            compatibility.verdict,
            DmabufScanoutVerdict::Incompatible(DmabufScanoutIncompatibility::RenderDeviceMismatch)
        );
    }

    #[test]
    fn compatibility_keeps_missing_layout_metadata_unknown_and_attemptable() {
        let compatibility = classify_dmabuf_scanout_compatibility(
            test_route(true),
            true,
            RenderDeviceMatch::Match,
            PrimeImportSupport::Supported,
            Vec::new(),
            true,
            &[TILED_A],
        );
        assert_eq!(
            compatibility.verdict,
            DmabufScanoutVerdict::Unknown(vec![DmabufScanoutUncertainty::NoAdvertisedSharedLayout])
        );
        assert!(!compatibility.verdict.blocks_attempt());
        assert_eq!(compatibility.linear, LinearScanoutSupport::None);
        assert_eq!(
            allocation_plans_for_route(true, &compatibility),
            vec![
                ScanoutAllocationPlan::ExplicitLinear,
                ScanoutAllocationPlan::LegacyLinear,
            ],
            "unknown metadata must retain the real fallback attempts"
        );
    }

    #[test]
    fn compatibility_keeps_prime_query_failure_unknown_and_attemptable() {
        let compatibility = classify_dmabuf_scanout_compatibility(
            test_route(true),
            true,
            RenderDeviceMatch::Match,
            PrimeImportSupport::Unknown,
            vec![TILED_A],
            true,
            &[TILED_A],
        );
        assert_eq!(
            compatibility.verdict,
            DmabufScanoutVerdict::Unknown(vec![
                DmabufScanoutUncertainty::KmsPrimeImportCapabilityUnavailable
            ])
        );
        assert!(!compatibility.verdict.blocks_attempt());
    }

    #[test]
    fn compatibility_rejects_missing_vulkan_external_memory_fd() {
        let compatibility = classify_dmabuf_scanout_compatibility(
            test_route(true),
            false,
            RenderDeviceMatch::Unknown,
            PrimeImportSupport::Unknown,
            Vec::new(),
            false,
            &[],
        );
        assert_eq!(
            compatibility.verdict,
            DmabufScanoutVerdict::Incompatible(
                DmabufScanoutIncompatibility::VulkanExternalMemoryFdUnavailable
            )
        );
        assert!(compatibility.verdict.blocks_attempt());
    }

    #[test]
    fn scanout_linear_policy_covers_dithering_drivers_not_amd() {
        use super::vk::DriverId;
        // HW-confirmed dithering on tiled scanout → must prefer LINEAR.
        assert!(scanout_prefers_linear(DriverId::NVIDIA_PROPRIETARY));
        assert!(scanout_prefers_linear(DriverId::INTEL_OPEN_SOURCE_MESA));
        // AMD needs tiled (RDNA4 LINEAR corrupts, issue #48) — must NOT prefer.
        assert!(!scanout_prefers_linear(DriverId::MESA_RADV));
        assert!(!scanout_prefers_linear(DriverId::AMD_PROPRIETARY));
        // Unconfirmed drivers stay on the tiled-first default.
        assert!(!scanout_prefers_linear(DriverId::MESA_LLVMPIPE));
    }

    // NVIDIA prefer_linear path — mirrors what GBM does on Pascal (GTX 1050):
    // LINEAR selected first even though tiled modifiers are also advertised.
    #[test]
    fn modifier_order_nvidia_prefers_linear_over_tiled() {
        // NVIDIA KMS advertises tiled first, then LINEAR — but prefer_linear=true
        // must put LINEAR at the front.
        let candidates = order_scanout_modifier_candidates(
            &[TILED_A, TILED_B, LINEAR],
            &[TILED_A, TILED_B, LINEAR],
            true,
            |_| true,
        );
        assert_eq!(candidates, vec![LINEAR, TILED_A, TILED_B]);
    }

    #[test]
    fn modifier_order_nvidia_linear_only_plane_yields_linear() {
        let candidates = order_scanout_modifier_candidates(&[LINEAR], &[LINEAR], true, |_| true);
        assert_eq!(candidates, vec![LINEAR]);
    }

    #[test]
    fn modifier_order_nvidia_no_linear_falls_back_to_tiled() {
        // If LINEAR is absent from the KMS plane, even prefer_linear=true should
        // yield the tiled modifiers (they're the only option).
        let candidates = order_scanout_modifier_candidates(
            &[TILED_A, TILED_B],
            &[TILED_A, TILED_B],
            true,
            |_| true,
        );
        assert_eq!(candidates, vec![TILED_A, TILED_B]);
    }

    #[test]
    fn fresh_bo_is_free() {
        let bo = BoState::default();
        assert_eq!(bo.phase, BoPhase::Free);
        assert!(bo.in_fence_fd.is_none());
        assert!(bo.release_fence_fd.is_none());
    }

    #[test]
    fn record_then_submit_transitions_to_submitted() {
        let mut bo = BoState::default();
        bo.transition_to_recording();
        assert_eq!(bo.phase, BoPhase::Recording);
        bo.transition_to_submitted(/* in_fence */ 42);
        assert_eq!(bo.phase, BoPhase::Submitted);
        assert_eq!(bo.in_fence_fd, Some(42));
    }

    #[test]
    fn submit_with_no_fence_sentinel_does_not_store_fd() {
        let mut bo = BoState::default();
        bo.transition_to_recording();
        bo.transition_to_submitted(/* no fence */ -1);
        assert_eq!(bo.phase, BoPhase::Submitted);
        assert!(bo.in_fence_fd.is_none());
    }

    #[test]
    fn atomic_accept_returns_in_fence_for_caller_to_close_and_stores_out_fence() {
        let mut bo = BoState::default();
        bo.transition_to_recording();
        bo.transition_to_submitted(42);
        let reclaimed = bo.transition_to_pending(/* out_fence */ 99);
        assert_eq!(bo.phase, BoPhase::Pending);
        assert_eq!(
            reclaimed,
            Some(42),
            "caller closes the in-fence fd; kernel only refs the sync_file"
        );
        assert!(bo.in_fence_fd.is_none(), "moved out into reclaimed");
        assert_eq!(bo.release_fence_fd, Some(99));
    }

    #[test]
    fn atomic_accept_with_no_out_fence_sentinel_does_not_store_release_fd() {
        let mut bo = BoState::default();
        bo.transition_to_recording();
        bo.transition_to_submitted(42);
        let reclaimed = bo.transition_to_pending(/* no out fence */ -1);
        assert_eq!(reclaimed, Some(42));
        assert!(bo.release_fence_fd.is_none());
    }

    #[test]
    fn atomic_reject_returns_to_recording_and_we_still_own_in_fence() {
        let mut bo = BoState::default();
        bo.transition_to_recording();
        bo.transition_to_submitted(42);
        let reclaimed = bo.transition_to_recording_after_atomic_reject();
        assert_eq!(bo.phase, BoPhase::Recording);
        assert_eq!(reclaimed, Some(42), "caller closes the fd");
        assert!(bo.in_fence_fd.is_none(), "moved out into reclaimed");
    }

    #[test]
    fn modeset_preempt_from_submitted_returns_in_fence() {
        let mut bo = BoState::default();
        bo.transition_to_recording();
        bo.transition_to_submitted(7);
        let in_fence = bo.transition_to_free_after_modeset_preempt();
        assert_eq!(bo.phase, BoPhase::Free);
        assert_eq!(in_fence, Some(7));
    }

    #[test]
    fn pending_then_onscreen_then_retiring_then_free_releases_fence() {
        let mut bo = BoState::default();
        bo.transition_to_recording();
        bo.transition_to_submitted(11);
        let _ = bo.transition_to_pending(22);
        assert_eq!(bo.phase, BoPhase::Pending);

        bo.transition_to_on_screen();
        assert_eq!(bo.phase, BoPhase::OnScreen);
        assert_eq!(
            bo.release_fence_fd,
            Some(22),
            "release fence stays attached while on-screen"
        );

        bo.transition_to_retiring();
        assert_eq!(bo.phase, BoPhase::Retiring);

        let release = bo.transition_to_free_after_retire();
        assert_eq!(bo.phase, BoPhase::Free);
        assert_eq!(release, Some(22), "caller closes the release fence");
        assert!(bo.release_fence_fd.is_none());
    }

    /// 4.1.2.7 fence-cycle integration test (host-pure variant per
    /// the plan: "mock the GPU side if Vulkan creation under
    /// lavapipe is awkward inside `cargo test`"). Drives a 3-bo
    /// pool through 6 frames in the steady-state cycle and asserts
    /// every fence fd issued is closed exactly once. No real GPU.
    /// The accounting catches state-machine bugs that leak fence
    /// fds (which is exactly the class of bug we hit on bare metal
    /// with the original IN_FENCE_FD ownership confusion).
    #[test]
    fn six_frames_cycle_through_pool_without_leaking_fences() {
        let mut bos: Vec<BoState> = (0..3).map(|_| BoState::default()).collect();
        let mut issued = 0u32;
        let mut closed = 0u32;
        let mut next_fd = 100i32;

        let alloc_fd = |issued: &mut u32, next_fd: &mut i32| -> i32 {
            *issued += 1;
            let fd = *next_fd;
            *next_fd += 1;
            fd
        };
        let close = |fd: Option<i32>, closed: &mut u32| {
            if fd.is_some() {
                *closed += 1;
            }
        };

        for _frame in 0..6 {
            // 1. Acquire Free bo and submit.
            let bo_idx = bos.iter().position(|b| b.phase == BoPhase::Free).expect(
                "with 3 bos and the cycle-advance below, at least one bo \
                 should be Free every frame",
            );
            let bo = &mut bos[bo_idx];
            bo.transition_to_recording();
            let in_fence = alloc_fd(&mut issued, &mut next_fd);
            bo.transition_to_submitted(in_fence);

            // 2. Atomic accept → Pending; closes the in-fence we just
            //    issued.
            let out_fence = alloc_fd(&mut issued, &mut next_fd);
            close(bo.transition_to_pending(out_fence), &mut closed);

            // 3. Pageflip-complete advance (mirrors
            //    `advance_pool_on_pageflip_complete` in backend.rs).
            let phases: Vec<BoPhase> = bos.iter().map(|b| b.phase).collect();
            for (i, phase) in phases.into_iter().enumerate() {
                match phase {
                    BoPhase::Retiring => {
                        close(bos[i].transition_to_free_after_retire(), &mut closed);
                    }
                    BoPhase::OnScreen => bos[i].transition_to_retiring(),
                    BoPhase::Pending => bos[i].transition_to_on_screen(),
                    _ => {}
                }
            }
        }

        // Drain remaining bos (simulates shutdown).
        for bo in &mut bos {
            let r = bo.transition_to_free_after_modeset_reset();
            close(r.in_fence, &mut closed);
            close(r.release_fence, &mut closed);
        }

        assert_eq!(
            issued, closed,
            "every fence fd issued must be closed exactly once \
             (issued={issued}, closed={closed})"
        );
        assert_eq!(
            issued, 12,
            "6 frames × (1 in_fence + 1 release_fence) = 12 fds expected"
        );
    }

    #[test]
    fn modeset_reset_returns_all_currently_held_fences() {
        // Pending: in-fence already returned to caller for closing,
        // live release fence still held by the bo.
        let mut bo = BoState::default();
        bo.transition_to_recording();
        bo.transition_to_submitted(5);
        let _ = bo.transition_to_pending(60);
        let released = bo.transition_to_free_after_modeset_reset();
        assert_eq!(bo.phase, BoPhase::Free);
        assert_eq!(released.in_fence, None);
        assert_eq!(released.release_fence, Some(60));

        // Submitted: still own the in-fence.
        let mut bo = BoState::default();
        bo.transition_to_recording();
        bo.transition_to_submitted(5);
        let released = bo.transition_to_free_after_modeset_reset();
        assert_eq!(released.in_fence, Some(5));
        assert_eq!(released.release_fence, None);

        // Recording: nothing held.
        let mut bo = BoState::default();
        bo.transition_to_recording();
        let released = bo.transition_to_free_after_modeset_reset();
        assert_eq!(released.in_fence, None);
        assert_eq!(released.release_fence, None);
    }

    #[test]
    fn addfb_modifier_flag_tracks_modifier_presence() {
        assert_eq!(addfb_flags_for_modifier(None), FbCmd2Flags::empty());
        assert_eq!(
            addfb_flags_for_modifier(Some(crate::kms::vk::dri3::DRM_FORMAT_MOD_LINEAR)),
            FbCmd2Flags::MODIFIERS
        );
    }
}
