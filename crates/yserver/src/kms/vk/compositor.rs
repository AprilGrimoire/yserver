//! Vulkan-side composite pass + types (sub-phase 4.1.3.4).
//!
//! Spec: docs/superpowers/specs/2026-05-07-phase4-1-vulkan-compositor-design.md
//! "Frame composite pass".
//!
//! Entry point: [`record_and_present_composite`] — records the
//! per-window composite into a `ScanoutBo` and atomic-flips it with
//! explicit IN_FENCE_FD / OUT_FENCE_PTR.

use std::os::fd::{FromRawFd, IntoRawFd};

use ash::vk;

use super::{
    device::VkContext,
    scanout::{BoPhase, ScanoutBo},
};
use crate::{
    drm::{Device as DrmDevice, page_flip::submit_flip_with_fences},
    platform::drm::Output,
};

/// Backend switch for which scanout path is active. Phase 4.1.5
/// retired the pixman alternatives — Vulkan composite is the sole
/// path. Kept as an enum so the `kms_xts_tooling` crate's `Default`
/// callers don't break their match arms.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum VkScanoutMode {
    /// Per-window composite (sub-phase 4.1.3.4): drawing ops fill
    /// per-drawable VkImage mirrors directly; the composite pass
    /// walks the window tree drawing one quad per visible drawable
    /// sampling its mirror, ending with an atomic flip with explicit
    /// IN/OUT fences.
    #[default]
    VkComposite,
}

/// Errors from the end-to-end Vulkan composite + atomic-flip path
/// (`record_and_present_composite`).
#[derive(Debug, thiserror::Error)]
pub enum PresentError {
    #[error("vulkan: {0}")]
    Vk(vk::Result),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("scanout bo has no DRM framebuffer (allocation incomplete?)")]
    NoFb,
    #[error("scanout bo state machine wrong phase: {0:?}")]
    WrongPhase(BoPhase),
}

impl From<vk::Result> for PresentError {
    fn from(r: vk::Result) -> Self {
        PresentError::Vk(r)
    }
}

// `record_and_present_composite` walks a [`CompositeScene`] (built
// by the backend from the window tree, in stacking order) and emits
// one textured quad per visible drawable into the target
// `ScanoutBo`. The atomic flip handshake — signalSemaphore on
// submit → exported as `IN_FENCE_FD` → `OUT_FENCE_PTR` adopted as
// release fence — is the fence model the design spec describes for
// "Per scanout / per CRTC".

use super::pipeline::{CompositePushConsts, CompositorPipeline};

/// One quad to draw in the composite pass. The backend assembles
/// these in the order they should rasterise (back-to-front).
#[derive(Debug, Clone, Copy)]
pub struct CompositeDraw {
    /// Mirror image view to sample. Must be in
    /// `SHADER_READ_ONLY_OPTIMAL` (the layout `MirrorUploader`
    /// leaves it in after the upload).
    pub image_view: vk::ImageView,
    /// Top-left corner of the destination rect in scanout pixel
    /// coords (after layout-offset translation by the caller).
    pub dst_origin: [f32; 2],
    /// Width × height of the destination rect.
    pub dst_size: [f32; 2],
    /// Source UV origin in normalised texture coords (0..1). For
    /// most draws this is `[0.0, 0.0]` (sample the whole texture);
    /// the bg_pixmap path sets it to the per-output slice of a
    /// virtual-screen-sized wallpaper.
    pub src_origin: [f32; 2],
    /// Source UV size in normalised texture coords (0..1). `[1, 1]`
    /// for the whole-texture case.
    pub src_size: [f32; 2],
    /// `true` selects the pass-through composite pipeline (the
    /// mirror's sampled α reaches the scanout's blend stage). Used
    /// by cursor + window-mirror draws post-L1 task A.16. `false`
    /// selects the force-opaque variant — the bg-pixmap root draw
    /// stays here because the root mirror is always fully painted
    /// and forcing α=1.0 sidesteps any α invariant on it.
    pub alpha_passthrough: bool,
}

/// One frame's worth of composite work for a single output. Built
/// fresh each frame by the backend.
#[derive(Debug, Clone)]
pub struct CompositeScene {
    /// `[r, g, b, a]` clear value for the scanout, in linear
    /// 0..1. Replaces pixman's "fill rect with bg_pixel" step.
    pub bg_color: [f32; 4],
    /// Draws in stacking order, back-to-front: bg pixmap (if any)
    /// first, then visible windows + descendants depth-first, then
    /// cursor last.
    pub draws: Vec<CompositeDraw>,
}

/// End-to-end composite + atomic flip for one frame.
///
/// Sequence (analogous to [`present_frame_via_vulkan`] but with a
/// real render pass instead of a buffer→image copy):
///
/// 1. Bo state must be `Free`. Transition to `Recording`.
/// 2. Allocate one descriptor set per draw from the passed
///    `descriptor_pool`. The pool was reset by the ring's
///    `release()` when this slot was last returned; it's empty.
/// 3. Record:
///    - layout transition `UNDEFINED → COLOR_ATTACHMENT_OPTIMAL`,
///    - `vkCmdBeginRendering` with `loadOp = CLEAR` (bg color),
///    - viewport + scissor = full bo,
///    - bind pipeline,
///    - per draw: bind descriptor, push consts, `vkCmdDraw(4, …)`,
///    - `vkCmdEndRendering`,
///    - layout transition `COLOR_ATTACHMENT_OPTIMAL → GENERAL`.
/// 4. `vkQueueSubmit2` with `signalSemaphore = bo.vk_semaphore`.
/// 5. Export the SYNC_FD payload, advance bo to `Submitted`.
/// 6. Atomic commit pageflip with the fd as `IN_FENCE_FD` +
///    `OUT_FENCE_PTR`. On accept: bo → `Pending` + adopt the
///    out-fence; on reject: revert to `Free`, close the held fd.
#[allow(clippy::too_many_arguments)]
#[allow(dead_code)]
pub(crate) fn record_and_present_composite(
    vk: &VkContext,
    drm: &DrmDevice,
    output: &Output,
    bo: &mut ScanoutBo,
    pipeline: &CompositorPipeline,
    descriptor_pool: vk::DescriptorPool,
    scene: &CompositeScene,
) -> Result<(), PresentError> {
    if bo.state.phase != BoPhase::Free {
        return Err(PresentError::WrongPhase(bo.state.phase));
    }
    let fb_handle = bo.fb_handle.ok_or(PresentError::NoFb)?;

    bo.state.transition_to_recording();

    // Reset is unnecessary — CompositePoolRing::release does the
    // reset when a slot is returned. Caller hands us an empty pool.

    let mut descriptors: Vec<vk::DescriptorSet> = Vec::with_capacity(scene.draws.len());
    for draw in &scene.draws {
        let layouts = [pipeline.descriptor_set_layout];
        let alloc_info = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(descriptor_pool)
            .set_layouts(&layouts);
        let set = match unsafe { vk.device.allocate_descriptor_sets(&alloc_info) } {
            Ok(sets) => sets[0],
            Err(e) => {
                log::warn!(
                    "composite: descriptor allocation failed ({e:?}) at draw {} of {} — \
                     remaining draws skipped this frame",
                    descriptors.len(),
                    scene.draws.len()
                );
                break;
            }
        };
        let image_info = [vk::DescriptorImageInfo::default()
            .image_view(draw.image_view)
            .sampler(pipeline.sampler)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
        let writes = [vk::WriteDescriptorSet::default()
            .dst_set(set)
            .dst_binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .image_info(&image_info)];
        unsafe { vk.device.update_descriptor_sets(&writes, &[]) };
        descriptors.push(set);
    }

    record_composite_command_buffer(vk, bo, pipeline, scene, &descriptors)?;

    // Submit (sync2). signalSemaphore = bo.vk_semaphore; KMS will
    // consume the exported SYNC_FD as IN_FENCE_FD.
    let cb = bo.vk_transfer.command_buffer;
    let cb_info = [vk::CommandBufferSubmitInfo::default().command_buffer(cb)];
    let sig_info = [vk::SemaphoreSubmitInfo::default()
        .semaphore(bo.vk_semaphore)
        .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)];
    let submit = [vk::SubmitInfo2::default()
        .command_buffer_infos(&cb_info)
        .signal_semaphore_infos(&sig_info)];
    unsafe {
        crate::vk_count!(queue_submit2);
        crate::vk_count!(submit_compositor);
        vk.device
            .queue_submit2(vk.graphics_queue, &submit, vk::Fence::null())?;
    }

    // Export SYNC_FD payload + advance bo state.
    let fd = bo
        .export_signaled_fd()
        .map_err(PresentError::Vk)?
        .map_or(-1, IntoRawFd::into_raw_fd);
    bo.state.transition_to_submitted(fd);

    // Atomic commit with explicit fences. Reuses the same
    // accept/reject handling as the PixmanShadow path —
    // identical fence-fd ownership invariant.
    let mut out_fence: i32 = -1;
    match submit_flip_with_fences(drm, output, fb_handle, fd, &mut out_fence) {
        Ok(()) => {
            if let Some(reclaimed) = bo.state.transition_to_pending(out_fence) {
                // SAFETY: `reclaimed` was inserted by
                // `transition_to_submitted` above. We're its only
                // owner.
                drop(unsafe { std::os::fd::OwnedFd::from_raw_fd(reclaimed) });
            }
            Ok(())
        }
        Err(e) => {
            if let Some(reclaimed) = bo.state.transition_to_recording_after_atomic_reject() {
                // SAFETY: same fd we just inserted.
                drop(unsafe { std::os::fd::OwnedFd::from_raw_fd(reclaimed) });
            }
            bo.state = super::scanout::BoState::default();
            Err(PresentError::Io(e))
        }
    }
}

#[allow(dead_code)]
fn record_composite_command_buffer(
    vk: &VkContext,
    bo: &ScanoutBo,
    pipeline: &CompositorPipeline,
    scene: &CompositeScene,
    descriptors: &[vk::DescriptorSet],
) -> Result<(), PresentError> {
    let device = &vk.device;
    let cb = bo.vk_transfer.command_buffer;

    unsafe {
        device.reset_command_buffer(cb, vk::CommandBufferResetFlags::empty())?;
        let begin = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        crate::vk_count!(begin_command_buffer);
        device.begin_command_buffer(cb, &begin)?;

        // 1. Layout transition: previous (we don't track between
        //    frames; UNDEFINED drops content but we always clear
        //    via load_op=CLEAR so that's fine) →
        //    COLOR_ATTACHMENT_OPTIMAL.
        let to_color = vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::TOP_OF_PIPE)
            .src_access_mask(vk::AccessFlags2::empty())
            .dst_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
            .dst_access_mask(vk::AccessFlags2::COLOR_ATTACHMENT_WRITE)
            .old_layout(vk::ImageLayout::UNDEFINED)
            .new_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .image(bo.vk_image)
            .subresource_range(
                vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .level_count(1)
                    .layer_count(1),
            );
        let to_color_arr = [to_color];
        let to_color_dep = vk::DependencyInfo::default().image_memory_barriers(&to_color_arr);
        crate::vk_count!(cmd_pipeline_barrier2);
        device.cmd_pipeline_barrier2(cb, &to_color_dep);

        // 2. Begin dynamic rendering with the bo's color view.
        let render_area = vk::Rect2D {
            offset: vk::Offset2D::default(),
            extent: vk::Extent2D {
                width: bo.width,
                height: bo.height,
            },
        };
        let color_attachment = [vk::RenderingAttachmentInfo::default()
            .image_view(bo.vk_image_view)
            .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .load_op(vk::AttachmentLoadOp::CLEAR)
            .store_op(vk::AttachmentStoreOp::STORE)
            .clear_value(vk::ClearValue {
                color: vk::ClearColorValue {
                    float32: scene.bg_color,
                },
            })];
        let rendering_info = vk::RenderingInfo::default()
            .render_area(render_area)
            .layer_count(1)
            .color_attachments(&color_attachment);
        crate::vk_count!(cmd_begin_rendering);
        device.cmd_begin_rendering(cb, &rendering_info);

        // 3. Dynamic state: viewport + scissor cover the whole bo.
        let viewport = [vk::Viewport {
            x: 0.0,
            y: 0.0,
            width: bo.width as f32,
            height: bo.height as f32,
            min_depth: 0.0,
            max_depth: 1.0,
        }];
        crate::vk_count!(cmd_set_viewport);
        device.cmd_set_viewport(cb, 0, &viewport);
        crate::vk_count!(cmd_set_scissor);
        device.cmd_set_scissor(cb, 0, &[render_area]);

        // 4. Per-draw: pick the alpha-mode pipeline variant
        //    (force-opaque vs pass-through), bind descriptor, push
        //    consts, draw 4 verts (TRIANGLE_STRIP quad). Pipeline
        //    rebinding between adjacent draws of the same variant is
        //    cheap on a same-handle redundant bind; if profiling
        //    shows it matters we can group draws by alpha mode.
        let viewport_size = [bo.width as f32, bo.height as f32];
        let mut last_pipeline: Option<vk::Pipeline> = None;
        for (i, draw) in scene.draws.iter().enumerate().take(descriptors.len()) {
            let pl = pipeline.pipeline_for(draw.alpha_passthrough);
            if last_pipeline != Some(pl) {
                crate::vk_count!(cmd_bind_pipeline);
                device.cmd_bind_pipeline(cb, vk::PipelineBindPoint::GRAPHICS, pl);
                last_pipeline = Some(pl);
            }
            let sets = [descriptors[i]];
            crate::vk_count!(cmd_bind_descriptor_sets);
            device.cmd_bind_descriptor_sets(
                cb,
                vk::PipelineBindPoint::GRAPHICS,
                pipeline.pipeline_layout,
                0,
                &sets,
                &[],
            );
            let push = CompositePushConsts {
                dst_origin: draw.dst_origin,
                dst_size: draw.dst_size,
                viewport: viewport_size,
                src_origin: draw.src_origin,
                src_size: draw.src_size,
            };
            crate::vk_count!(cmd_push_constants);
            device.cmd_push_constants(
                cb,
                pipeline.pipeline_layout,
                vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                0,
                push.as_bytes(),
            );
            crate::vk_count!(cmd_draw);
            device.cmd_draw(cb, 4, 1, 0, 0);
        }

        crate::vk_count!(cmd_end_rendering);
        device.cmd_end_rendering(cb);

        // 6. Layout transition: COLOR_ATTACHMENT_OPTIMAL → GENERAL
        //    for KMS scanout (same layout the PixmanShadow path
        //    leaves it in).
        let to_scanout = vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
            .src_access_mask(vk::AccessFlags2::COLOR_ATTACHMENT_WRITE)
            .dst_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
            .dst_access_mask(vk::AccessFlags2::empty())
            .old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .new_layout(vk::ImageLayout::GENERAL)
            .image(bo.vk_image)
            .subresource_range(
                vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .level_count(1)
                    .layer_count(1),
            );
        let to_scanout_arr = [to_scanout];
        let to_scanout_dep = vk::DependencyInfo::default().image_memory_barriers(&to_scanout_arr);
        crate::vk_count!(cmd_pipeline_barrier2);
        device.cmd_pipeline_barrier2(cb, &to_scanout_dep);

        crate::vk_count!(end_command_buffer);
        device.end_command_buffer(cb)?;
    }
    Ok(())
}
