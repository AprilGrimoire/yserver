//! Convolution-pipeline construction + UBO-write smoke test (phase
//! 2 of the convolution-filter work,
//! `docs/superpowers/plans/2026-05-15-render-convolution-filter.md`
//! task T5).
//!
//! Builds a [`ConvolutionPipeline`] against a real Vulkan device,
//! opens a [`BatchUploadArena`], allocates a kernel UBO slot, memcpys
//! a 5×5 box-blur kernel into it, and confirms the bytes hit the
//! host-coherent mapping. Validates the wire-up between the new
//! pipeline, the arena's `UNIFORM_BUFFER` usage flag, and
//! `ConvolutionKernelData::as_bytes`.
//!
//! Marked `#[ignore]` because it needs a working Vulkan ICD; run
//! with `cargo test -p yserver --test convolution_pipeline_smoke --
//! --ignored` under vng / on bare metal.
//!
//! [`ConvolutionPipeline`]: yserver::kms::vk::convolution_pipeline::ConvolutionPipeline
//! [`BatchUploadArena`]: yserver::kms::scheduler::batch_upload_arena::BatchUploadArena

#![cfg(target_os = "linux")]

#[test]
#[ignore = "needs live Vulkan ICD"]
fn convolution_pipeline_builds_and_ubo_alloc_writes_kernel_bytes() {
    use yserver::kms::{
        scheduler::batch_upload_arena::BatchUploadArena,
        vk::{
            convolution_pipeline::{
                ConvolutionKernelData, ConvolutionPipeline, KERNEL_UBO_ALIGNMENT,
            },
            device::VkContext,
        },
    };

    let vk = VkContext::new().expect("VkContext init failed — install lavapipe or run under vng");
    // Pipeline construction. Failure here means the SPIR-V doesn't
    // load or the device rejected the pipeline layout — both bugs in
    // the new code, not environment.
    let _pipeline =
        ConvolutionPipeline::new(std::sync::Arc::clone(&vk)).expect("ConvolutionPipeline::new");

    // UBO write smoke. Allocate a kernel slot from a fresh
    // BatchUploadArena (matching the production per-batch pattern),
    // memcpy a 5×5 box-blur kernel, then read back through the
    // host-coherent mapping.
    let kernel = ConvolutionKernelData::from_parsed(5, 5, &[1.0_f32 / 25.0; 25])
        .expect("5×5 kernel in range");
    let mut arena = BatchUploadArena::new(std::sync::Arc::clone(&vk));
    let kernel_size = std::mem::size_of::<ConvolutionKernelData>() as u64;
    let alloc = arena
        .alloc(kernel_size, KERNEL_UBO_ALIGNMENT)
        .expect("arena alloc");
    assert!(
        alloc.offset.is_multiple_of(KERNEL_UBO_ALIGNMENT),
        "UBO offset {} must be {KERNEL_UBO_ALIGNMENT}-aligned",
        alloc.offset,
    );
    assert_eq!(alloc.size, kernel_size);

    // SAFETY: `mapped_ptr` is mapped for `alloc.size` bytes, and we
    // copy exactly `kernel_size` (== alloc.size).
    unsafe {
        std::ptr::copy_nonoverlapping(
            kernel.as_bytes().as_ptr(),
            alloc.mapped_ptr.as_ptr(),
            kernel_size as usize,
        );
    }
    // Read back through the same host-coherent mapping.
    // SAFETY: same span as the write; no aliasing because we own the
    // arena exclusively and no GPU work has been submitted.
    let readback: &[u8] =
        unsafe { std::slice::from_raw_parts(alloc.mapped_ptr.as_ptr(), kernel_size as usize) };
    let expected = kernel.as_bytes();
    assert_eq!(readback.len(), expected.len());
    // dims at the first 8 bytes — width then height.
    assert_eq!(&readback[0..4], &5u32.to_ne_bytes());
    assert_eq!(&readback[4..8], &5u32.to_ne_bytes());
    // First weight at offset 8.
    assert_eq!(&readback[8..12], &(1.0_f32 / 25.0).to_ne_bytes());
    // Trailing weights past the populated kernel must be zero (the
    // shader's static loop bound is `kernel.dims.x * kernel.dims.y`
    // so the tail is unread, but `from_parsed` zeros it for
    // determinism).
    let tail_offset = 8 + 25 * 4;
    assert!(
        readback[tail_offset..].iter().all(|&b| b == 0),
        "trailing weights past the 25-element kernel must be zero"
    );
    // Full bitwise match.
    assert_eq!(readback, expected);
}
