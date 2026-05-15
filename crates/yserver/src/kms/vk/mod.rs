//! Vulkan backend for the KMS compositor (Phase 4.1).
//!
//! Spec: docs/superpowers/specs/2026-05-07-phase4-1-vulkan-compositor-design.md
//!
//! Sub-phase 4.1.1: instance/device/allocator init, idle. Drawing
//! still runs through pixman; this module brings up Vulkan in
//! parallel.

pub mod call_stats;
pub mod compositor;
pub mod convolution_pipeline;
pub mod copy_scratch;
pub mod device;
pub mod dri3;
pub mod dst_readback;
pub mod glyph;
pub mod gradient;
pub mod instance;
pub mod logic_fill_pipeline;
pub mod mask_scratch;
pub mod memory;
pub mod ops;
pub mod pipeline;
pub mod pixmap_pool;
pub mod render_pipeline;
pub mod scanout;
pub mod sync;
pub mod target;
pub mod text_pipeline;
pub mod trap_pipeline;
// `pub mod upload;` retired in 4.1.5 — pixman → mirror upload pump
// gone with the rest of the pixman canonical-store machinery.
