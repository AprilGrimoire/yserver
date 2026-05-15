//! Pipeline for X RENDER `SetPictureFilter convolution` (phase 2 of
//! the convolution-filter work, plan
//! `docs/superpowers/plans/2026-05-15-render-convolution-filter.md`).
//!
//! Phase 1 (`898dd19`) parsed `SetPictureFilter` and stored the
//! kernel on `PictureState::Drawable::filter`; this module gives the
//! kernel a rendering effect.
//!
//! Used by xfwm4 / marco / mutter / picom for window-shadow Gaussian
//! blur. Without this path the parsed kernel is ignored and shadows
//! render as hard-edged flat rectangles.
//!
//! ## Scope (phase 2)
//!
//! - Single pipeline keyed on `(PictOp::Over, dst_format =
//!   B8G8R8A8_UNORM)`. The caller falls back to the standard RENDER
//!   pipeline for any other (op, format) combination — extending to
//!   more formats or PictOps is mechanical when a real client needs
//!   it (the shader is format-agnostic; the cache key is the limit).
//! - One source sampler binding + one kernel UBO binding. No mask,
//!   no dst-readback (those would need their own variants).
//! - Caller gates on identity transform + Repeat::None + no
//!   alpha_map — out-of-scope cases fall back to the standard
//!   pipeline at LINEAR filter quality.
//!
//! Push constants reuse [`super::render_pipeline::RenderPushConsts`]
//! (same 128-byte layout); the fragment shader ignores the mask
//! fields.

use std::sync::Arc;

use ash::vk;

use super::{device::VkContext, render_pipeline::RenderPushConsts};

const VERTEX_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/render.vert.spv"));
const FRAGMENT_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/convolution.frag.spv"));

/// Max kernel dimension (square cap). Real xfwm4 / picom shadow
/// blurs are 5×5..11×11; 21×21 leaves comfortable headroom. The
/// matching GLSL constant in `convolution.frag.glsl` must stay in
/// sync — a static check at the bottom of the module verifies the
/// UBO size matches what the shader declares.
pub const MAX_KERNEL_DIM: usize = 21;
pub const MAX_KERNEL_ELEMS: usize = MAX_KERNEL_DIM * MAX_KERNEL_DIM;

/// Conservative alignment for the kernel UBO offset inside a
/// `BatchUploadArena` slice. The Vulkan spec caps
/// `minUniformBufferOffsetAlignment` at 256 bytes for all
/// implementations we care about (AMD/Intel/NVIDIA/llvmpipe all
/// report ≤256), so this satisfies the binding requirement
/// without per-device introspection. 256 bytes wasted in the worst
/// case per descriptor — negligible against the 1 MiB+ chunk size.
pub const KERNEL_UBO_ALIGNMENT: u64 = 256;

/// UBO contents bound at descriptor set 0, binding 1. The
/// `#[repr(C)]` layout matches the shader's
/// `layout(scalar) uniform Kernel { uvec2 dims; float weights[…]; }`
/// — `VK_EXT_scalar_block_layout` (core in 1.2, enabled at
/// device-create) lets the float array pack tightly without the
/// std140 vec4 stride.
///
/// Total size: 8 + 4×441 = 1772 bytes. Comfortably under any
/// `maxUniformBufferRange` (16 KiB minimum guarantee).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ConvolutionKernelData {
    pub dims: [u32; 2],
    pub weights: [f32; MAX_KERNEL_ELEMS],
}

const _: () = assert!(std::mem::size_of::<ConvolutionKernelData>() == 8 + 4 * MAX_KERNEL_ELEMS);

impl ConvolutionKernelData {
    /// Zero-initialised kernel. Callers populate `dims` and the
    /// leading `dims.0 * dims.1` weight entries before writing into
    /// the UBO; the tail of `weights` stays zero and is unread by
    /// the shader (the kernel-size loop is bounded by `dims`).
    #[must_use]
    pub fn zeroed() -> Self {
        Self {
            dims: [0, 0],
            weights: [0.0; MAX_KERNEL_ELEMS],
        }
    }

    /// Build a kernel UBO image from `(width, height, weights)` as
    /// stored in [`super::super::backend`]'s
    /// `PictureFilter::Convolution`. Returns `None` if the kernel is
    /// out of range (zero-sized, larger than the shader's static cap,
    /// or even-dimensioned) — caller should fall back to the standard
    /// pipeline.
    ///
    /// Even dimensions are rejected because the shader (and pixman's
    /// reference convolution) anchors the kernel on the centre texel
    /// at `dims / 2` and loops `[-half, +half]` inclusive, which only
    /// produces a symmetric footprint for odd dims. Real RENDER
    /// consumers (xfwm4, picom) only emit odd-dim kernels; supporting
    /// even dims would need corner-anchored sampling semantics that
    /// the X RENDER spec doesn't pin down.
    #[must_use]
    pub fn from_parsed(width: u16, height: u16, weights: &[f32]) -> Option<Self> {
        let w = usize::from(width);
        let h = usize::from(height);
        if w == 0 || h == 0 || w > MAX_KERNEL_DIM || h > MAX_KERNEL_DIM {
            return None;
        }
        if w.is_multiple_of(2) || h.is_multiple_of(2) {
            return None;
        }
        let n = w * h;
        if weights.len() != n {
            return None;
        }
        let mut k = Self::zeroed();
        k.dims = [width.into(), height.into()];
        k.weights[..n].copy_from_slice(weights);
        Some(k)
    }

    /// View this struct as a byte slice for direct memcpy into the
    /// host-visible UBO mapping.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        // SAFETY: `repr(C)`, plain `u32` / `f32` fields, no padding
        // (asserted at module scope).
        unsafe {
            std::slice::from_raw_parts(
                std::ptr::from_ref::<Self>(self).cast::<u8>(),
                std::mem::size_of::<Self>(),
            )
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConvolutionPipelineError {
    #[error("vulkan: {0:?}")]
    Vk(vk::Result),
    #[error(
        "convolution shader SPIR-V from build.rs is malformed (length not multiple of 4): {0} \
         bytes"
    )]
    SpirvUnaligned(usize),
}

impl From<vk::Result> for ConvolutionPipelineError {
    fn from(r: vk::Result) -> Self {
        ConvolutionPipelineError::Vk(r)
    }
}

/// Pipeline + descriptor scaffolding for the convolution Composite
/// path. Built once at backend init; the pipeline itself is fixed at
/// `(PictOp::Over, B8G8R8A8_UNORM)` for phase 2.
pub struct ConvolutionPipeline {
    vk: Arc<VkContext>,
    pipeline_layout: vk::PipelineLayout,
    descriptor_set_layout: vk::DescriptorSetLayout,
    sampler: vk::Sampler,
    pipeline: vk::Pipeline,
}

impl std::fmt::Debug for ConvolutionPipeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConvolutionPipeline")
            .field("pipeline_layout", &self.pipeline_layout)
            .field("descriptor_set_layout", &self.descriptor_set_layout)
            .field("pipeline", &self.pipeline)
            .finish_non_exhaustive()
    }
}

impl ConvolutionPipeline {
    pub fn new(vk: Arc<VkContext>) -> Result<Self, ConvolutionPipelineError> {
        let device = &vk.device;

        // NEAREST + CLAMP_TO_EDGE. Each kernel tap samples a single
        // source texel at integer offsets from the centre, so LINEAR
        // would degenerate to the same result at the cost of a
        // four-tap filter per sample. CLAMP_TO_EDGE matches Repeat::
        // None for the typical shadow-pixmap case (transparent edge
        // ring).
        let sampler_info = vk::SamplerCreateInfo::default()
            .mag_filter(vk::Filter::NEAREST)
            .min_filter(vk::Filter::NEAREST)
            .mipmap_mode(vk::SamplerMipmapMode::NEAREST)
            .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .min_lod(0.0)
            .max_lod(0.0);
        let sampler = unsafe { device.create_sampler(&sampler_info, None)? };

        let dsl_bindings = [
            vk::DescriptorSetLayoutBinding::default()
                .binding(0)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(1)
                .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
        ];
        let dsl_info = vk::DescriptorSetLayoutCreateInfo::default().bindings(&dsl_bindings);
        let descriptor_set_layout =
            match unsafe { device.create_descriptor_set_layout(&dsl_info, None) } {
                Ok(d) => d,
                Err(e) => {
                    unsafe { device.destroy_sampler(sampler, None) };
                    return Err(e.into());
                }
            };

        let set_layouts = [descriptor_set_layout];
        let push_const_ranges = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT)
            .offset(0)
            .size(std::mem::size_of::<RenderPushConsts>() as u32)];
        let pl_info = vk::PipelineLayoutCreateInfo::default()
            .set_layouts(&set_layouts)
            .push_constant_ranges(&push_const_ranges);
        let pipeline_layout = match unsafe { device.create_pipeline_layout(&pl_info, None) } {
            Ok(p) => p,
            Err(e) => {
                unsafe {
                    device.destroy_descriptor_set_layout(descriptor_set_layout, None);
                    device.destroy_sampler(sampler, None);
                }
                return Err(e.into());
            }
        };

        let pipeline = match build_pipeline(&vk, pipeline_layout) {
            Ok(p) => p,
            Err(e) => {
                unsafe {
                    device.destroy_pipeline_layout(pipeline_layout, None);
                    device.destroy_descriptor_set_layout(descriptor_set_layout, None);
                    device.destroy_sampler(sampler, None);
                }
                return Err(e);
            }
        };

        Ok(Self {
            vk,
            pipeline_layout,
            descriptor_set_layout,
            sampler,
            pipeline,
        })
    }

    pub fn pipeline_layout(&self) -> vk::PipelineLayout {
        self.pipeline_layout
    }

    pub fn pipeline(&self) -> vk::Pipeline {
        self.pipeline
    }

    /// Allocate a descriptor set out of `arena` and bind `src_view`
    /// (binding 0) + the kernel UBO sub-range (binding 1). The UBO
    /// lives in a `BatchUploadArena` chunk owned by the same paint
    /// batch as the descriptor — `BatchUploadArena` chunks survive
    /// until batch retirement, so the binding stays valid until the
    /// CB completes. Caller must memcpy the kernel bytes into the
    /// mapping before `vkQueueSubmit` runs the CB.
    pub fn allocate_descriptor_for_views_into(
        &self,
        arena: &mut crate::kms::scheduler::batch_descriptor_arena::BatchDescriptorArena,
        src_view: vk::ImageView,
        kernel_buffer: vk::Buffer,
        kernel_offset: u64,
        kernel_size: u64,
    ) -> Result<vk::DescriptorSet, vk::Result> {
        let set = arena.allocate_set(self.descriptor_set_layout)?;
        let src_info = [vk::DescriptorImageInfo::default()
            .image_view(src_view)
            .sampler(self.sampler)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
        let buffer_info = [vk::DescriptorBufferInfo::default()
            .buffer(kernel_buffer)
            .offset(kernel_offset)
            .range(kernel_size)];
        let writes = [
            vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(&src_info),
            vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(1)
                .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                .buffer_info(&buffer_info),
        ];
        unsafe { self.vk.device.update_descriptor_sets(&writes, &[]) };
        Ok(set)
    }
}

impl Drop for ConvolutionPipeline {
    fn drop(&mut self) {
        unsafe {
            let _ = self.vk.device.queue_wait_idle(self.vk.graphics_queue);
            self.vk.device.destroy_pipeline(self.pipeline, None);
            self.vk
                .device
                .destroy_pipeline_layout(self.pipeline_layout, None);
            self.vk
                .device
                .destroy_descriptor_set_layout(self.descriptor_set_layout, None);
            self.vk.device.destroy_sampler(self.sampler, None);
        }
    }
}

fn build_pipeline(
    vk: &VkContext,
    pipeline_layout: vk::PipelineLayout,
) -> Result<vk::Pipeline, ConvolutionPipelineError> {
    let device = &vk.device;
    let vert_module = create_shader_module(device, VERTEX_SPV)?;
    let frag_module = match create_shader_module(device, FRAGMENT_SPV) {
        Ok(m) => m,
        Err(e) => {
            unsafe { device.destroy_shader_module(vert_module, None) };
            return Err(e);
        }
    };

    let entry = c"main";
    let stages = [
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::VERTEX)
            .module(vert_module)
            .name(entry),
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::FRAGMENT)
            .module(frag_module)
            .name(entry),
    ];

    let vertex_input = vk::PipelineVertexInputStateCreateInfo::default();
    let input_assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
        .topology(vk::PrimitiveTopology::TRIANGLE_STRIP);
    let viewport_state = vk::PipelineViewportStateCreateInfo::default()
        .viewport_count(1)
        .scissor_count(1);
    let rasterization = vk::PipelineRasterizationStateCreateInfo::default()
        .polygon_mode(vk::PolygonMode::FILL)
        .cull_mode(vk::CullModeFlags::NONE)
        .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
        .line_width(1.0);
    let multisample = vk::PipelineMultisampleStateCreateInfo::default()
        .rasterization_samples(vk::SampleCountFlags::TYPE_1);

    // PictOp::Over fixed-function factors for an opaque (no-alpha)
    // BGRA destination: `(ONE, ONE_MINUS_SRC_ALPHA)`. Phase 2 only
    // targets this combo; broader format support extends the cache
    // key.
    let color_blend_attachments = [vk::PipelineColorBlendAttachmentState::default()
        .blend_enable(true)
        .src_color_blend_factor(vk::BlendFactor::ONE)
        .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
        .color_blend_op(vk::BlendOp::ADD)
        .src_alpha_blend_factor(vk::BlendFactor::ONE)
        .dst_alpha_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
        .alpha_blend_op(vk::BlendOp::ADD)
        .color_write_mask(vk::ColorComponentFlags::RGBA)];
    let color_blend =
        vk::PipelineColorBlendStateCreateInfo::default().attachments(&color_blend_attachments);

    let dynamic_state_array = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
    let dynamic_state =
        vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_state_array);
    let color_formats = [vk::Format::B8G8R8A8_UNORM];
    let mut rendering_info =
        vk::PipelineRenderingCreateInfo::default().color_attachment_formats(&color_formats);

    let pipeline_info = vk::GraphicsPipelineCreateInfo::default()
        .stages(&stages)
        .vertex_input_state(&vertex_input)
        .input_assembly_state(&input_assembly)
        .viewport_state(&viewport_state)
        .rasterization_state(&rasterization)
        .multisample_state(&multisample)
        .color_blend_state(&color_blend)
        .dynamic_state(&dynamic_state)
        .layout(pipeline_layout)
        .push_next(&mut rendering_info);

    let pipeline = match unsafe {
        device.create_graphics_pipelines(vk::PipelineCache::null(), &[pipeline_info], None)
    } {
        Ok(ps) => ps[0],
        Err((_, e)) => {
            unsafe {
                device.destroy_shader_module(vert_module, None);
                device.destroy_shader_module(frag_module, None);
            }
            return Err(e.into());
        }
    };
    unsafe {
        device.destroy_shader_module(vert_module, None);
        device.destroy_shader_module(frag_module, None);
    }
    Ok(pipeline)
}

fn create_shader_module(
    device: &ash::Device,
    spv_bytes: &[u8],
) -> Result<vk::ShaderModule, ConvolutionPipelineError> {
    if !spv_bytes.len().is_multiple_of(4) {
        return Err(ConvolutionPipelineError::SpirvUnaligned(spv_bytes.len()));
    }
    let mut code: Vec<u32> = Vec::with_capacity(spv_bytes.len() / 4);
    for chunk in spv_bytes.chunks_exact(4) {
        code.push(u32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    let info = vk::ShaderModuleCreateInfo::default().code(&code);
    Ok(unsafe { device.create_shader_module(&info, None)? })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kernel_data_size_matches_shader_layout() {
        // 8 bytes for `uvec2 dims` + 4 bytes × 441 weight entries =
        // 1772 bytes. Must match the `layout(scalar) Kernel` block
        // declared in convolution.frag.glsl.
        assert_eq!(std::mem::size_of::<ConvolutionKernelData>(), 1772);
    }

    #[test]
    fn zeroed_kernel_is_all_zero() {
        let k = ConvolutionKernelData::zeroed();
        assert_eq!(k.dims, [0, 0]);
        assert!(k.weights.iter().all(|&w| w == 0.0));
        let bytes = k.as_bytes();
        assert_eq!(bytes.len(), 1772);
        assert!(bytes.iter().all(|&b| b == 0));
    }

    #[test]
    fn from_parsed_copies_dims_and_leading_weights() {
        let weights: Vec<f32> = (0..9).map(|i| i as f32 * 0.5).collect();
        let k = ConvolutionKernelData::from_parsed(3, 3, &weights).expect("3x3 in range");
        assert_eq!(k.dims, [3, 3]);
        assert_eq!(&k.weights[..9], weights.as_slice());
        // Tail past the populated kernel stays zero.
        assert!(k.weights[9..].iter().all(|&w| w == 0.0));
    }

    #[test]
    fn from_parsed_rejects_oversized_kernel() {
        let weights = vec![0.0_f32; 23 * 23];
        assert!(ConvolutionKernelData::from_parsed(23, 23, &weights).is_none());
    }

    #[test]
    fn from_parsed_rejects_zero_dim() {
        assert!(ConvolutionKernelData::from_parsed(0, 3, &[]).is_none());
        assert!(ConvolutionKernelData::from_parsed(3, 0, &[]).is_none());
    }

    #[test]
    fn from_parsed_rejects_weight_count_mismatch() {
        let weights = vec![1.0_f32; 8]; // expecting 9 for 3x3
        assert!(ConvolutionKernelData::from_parsed(3, 3, &weights).is_none());
    }

    #[test]
    fn from_parsed_rejects_even_dimensions() {
        // The shader anchors at `dims / 2` and loops inclusively, so
        // a 2x2 kernel would sample 3x3 with three tail-zero weights.
        // Reject up front so the caller falls through to LINEAR.
        let weights_2x2 = vec![0.25_f32; 4];
        assert!(ConvolutionKernelData::from_parsed(2, 2, &weights_2x2).is_none());
        let weights_2x3 = vec![0.25_f32; 6];
        assert!(ConvolutionKernelData::from_parsed(2, 3, &weights_2x3).is_none());
        let weights_3x4 = vec![0.25_f32; 12];
        assert!(ConvolutionKernelData::from_parsed(3, 4, &weights_3x4).is_none());
    }

    #[test]
    fn as_bytes_roundtrip_dims_at_offset_zero() {
        let k = ConvolutionKernelData::from_parsed(5, 7, &[0.25; 35]).expect("5x7 in range");
        let bytes = k.as_bytes();
        assert_eq!(&bytes[0..4], &5u32.to_ne_bytes());
        assert_eq!(&bytes[4..8], &7u32.to_ne_bytes());
        // First weight lands immediately after the 8-byte uvec2.
        assert_eq!(&bytes[8..12], &0.25_f32.to_ne_bytes());
    }
}
