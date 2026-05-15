//! Host-visible, append-only upload arena owned by `PaintBatch`.
//!
//! Returns stable `(buffer, offset, mapped_ptr, size)` quadruples
//! that remain valid until the batch retires. Chunked: a single
//! buffer would either be wastefully large for small batches or
//! invalidate offsets on grow. Per-chunk allocation is bumped from
//! `current_offset`; new chunks are added when the active chunk
//! can't fit the requested size.
//!
//! Owned by `PaintBatch`. Released at batch retirement (Retired or
//! Poisoned) via the `BatchResource` trait — each chunk's
//! `VkBuffer + VkDeviceMemory + mapping` is destroyed.

use std::{ptr::NonNull, sync::Arc};

use ash::vk;

use crate::kms::{scheduler::paint_batch::BatchResource, vk::device::VkContext};

#[derive(Debug, thiserror::Error)]
pub enum ArenaError {
    #[error("vulkan: {0:?}")]
    Vk(vk::Result),
    #[error("no host-visible host-coherent memory type")]
    NoMemoryType,
}

impl From<vk::Result> for ArenaError {
    fn from(r: vk::Result) -> Self {
        ArenaError::Vk(r)
    }
}

/// A stable allocation within a batch. Valid until batch retirement.
#[derive(Debug, Clone, Copy)]
pub struct UploadAllocation {
    pub buffer: vk::Buffer,
    pub offset: u64,
    pub size: u64,
    /// CPU-mapped pointer at `buffer + offset`. Host-coherent, no
    /// flush needed. Caller writes via `copy_nonoverlapping`.
    pub mapped_ptr: NonNull<u8>,
}

// SAFETY: `NonNull<u8>` is not `Send` by default. The KMS backend's
// single-threaded-core invariant (phase 6.8) guarantees these
// allocations are never moved across threads; the mapping is owned
// by the same thread that issues paint records and reads them back
// from staging.
unsafe impl Send for UploadAllocation {}

#[derive(Debug)]
struct Chunk {
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    base_ptr: NonNull<u8>,
    size: u64,
    /// Bytes used in this chunk. Monotonic within the batch.
    used: u64,
}

// SAFETY: same as `UploadAllocation` — the single-threaded-core
// invariant (phase 6.8) keeps chunks pinned to the backend thread.
unsafe impl Send for Chunk {}

pub struct BatchUploadArena {
    vk: Arc<VkContext>,
    chunks: Vec<Chunk>,
}

impl std::fmt::Debug for BatchUploadArena {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BatchUploadArena")
            .field("chunks", &self.chunks.len())
            .finish_non_exhaustive()
    }
}

const MIN_CHUNK_SIZE: u64 = 1024 * 1024; // 1 MiB
const MAX_CHUNK_SIZE: u64 = 64 * 1024 * 1024; // 64 MiB

impl BatchUploadArena {
    pub fn new(vk: Arc<VkContext>) -> Self {
        Self {
            vk,
            chunks: Vec::new(),
        }
    }

    /// Allocate `size` bytes aligned to `alignment` (must be a
    /// power of two). Returns a stable allocation; the
    /// `mapped_ptr` is writable until batch retirement.
    pub fn alloc(&mut self, size: u64, alignment: u64) -> Result<UploadAllocation, ArenaError> {
        debug_assert!(alignment.is_power_of_two(), "alignment not pow2");
        if size == 0 {
            return Err(ArenaError::Vk(vk::Result::ERROR_VALIDATION_FAILED_EXT));
        }

        // Try to fit in the active chunk.
        if let Some(chunk) = self.chunks.last_mut() {
            let aligned = (chunk.used + alignment - 1) & !(alignment - 1);
            if aligned + size <= chunk.size {
                let offset = aligned;
                chunk.used = aligned + size;
                // SAFETY: chunk.base_ptr is mapped for the full
                // chunk size, and offset+size ≤ chunk.size.
                let mapped_ptr =
                    unsafe { NonNull::new_unchecked(chunk.base_ptr.as_ptr().add(offset as usize)) };
                return Ok(UploadAllocation {
                    buffer: chunk.buffer,
                    offset,
                    size,
                    mapped_ptr,
                });
            }
        }

        // Grow: allocate a new chunk. Doubles up to MAX_CHUNK_SIZE
        // (caps a single chunk at 64 MiB); falls back to
        // MIN_CHUNK_SIZE on first alloc; always at least `size` so a
        // single large request never gets an undersized chunk.
        let next_size = self
            .chunks
            .last()
            .map(|c| (c.size * 2).min(MAX_CHUNK_SIZE))
            .unwrap_or(MIN_CHUNK_SIZE)
            .max(size);
        let chunk = Self::allocate_chunk(&self.vk, next_size)?;
        let mapped_ptr = chunk.base_ptr;
        let buffer = chunk.buffer;
        let mut chunk = chunk;
        chunk.used = size;
        self.chunks.push(chunk);
        Ok(UploadAllocation {
            buffer,
            offset: 0,
            size,
            mapped_ptr,
        })
    }

    fn allocate_chunk(vk: &VkContext, size: u64) -> Result<Chunk, ArenaError> {
        let buf_info = vk::BufferCreateInfo::default()
            .size(size)
            .usage(
                vk::BufferUsageFlags::TRANSFER_SRC
                    | vk::BufferUsageFlags::VERTEX_BUFFER
                    | vk::BufferUsageFlags::UNIFORM_BUFFER,
            )
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let buffer = unsafe { vk.device.create_buffer(&buf_info, None)? };

        let mem_reqs = unsafe { vk.device.get_buffer_memory_requirements(buffer) };
        let mem_props = unsafe {
            vk.instance
                .get_physical_device_memory_properties(vk.physical_device)
        };
        let want = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
        let mt = (0..mem_props.memory_type_count).find(|&i| {
            mem_reqs.memory_type_bits & (1 << i) != 0
                && mem_props.memory_types[i as usize]
                    .property_flags
                    .contains(want)
        });
        let Some(mt) = mt else {
            unsafe { vk.device.destroy_buffer(buffer, None) };
            return Err(ArenaError::NoMemoryType);
        };
        let alloc_info = vk::MemoryAllocateInfo::default()
            .allocation_size(mem_reqs.size)
            .memory_type_index(mt);
        let memory = match unsafe { vk.device.allocate_memory(&alloc_info, None) } {
            Ok(m) => m,
            Err(e) => {
                unsafe { vk.device.destroy_buffer(buffer, None) };
                return Err(ArenaError::Vk(e));
            }
        };
        if let Err(e) = unsafe { vk.device.bind_buffer_memory(buffer, memory, 0) } {
            unsafe {
                vk.device.free_memory(memory, None);
                vk.device.destroy_buffer(buffer, None);
            }
            return Err(ArenaError::Vk(e));
        }
        let mapped = match unsafe {
            vk.device
                .map_memory(memory, 0, size, vk::MemoryMapFlags::empty())
        } {
            Ok(p) => p,
            Err(e) => {
                unsafe {
                    vk.device.free_memory(memory, None);
                    vk.device.destroy_buffer(buffer, None);
                }
                return Err(ArenaError::Vk(e));
            }
        };
        let base_ptr = NonNull::new(mapped.cast::<u8>()).expect("vkMapMemory non-null");
        Ok(Chunk {
            buffer,
            memory,
            base_ptr,
            size,
            used: 0,
        })
    }
}

impl BatchResource for BatchUploadArena {
    fn release(self: Box<Self>, vk: &VkContext) {
        for chunk in self.chunks {
            unsafe {
                vk.device.unmap_memory(chunk.memory);
                vk.device.destroy_buffer(chunk.buffer, None);
                vk.device.free_memory(chunk.memory, None);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use super::*;

    // The allocator's bump arithmetic is testable without Vk:
    // factor the alignment-and-fit math into a pure helper, then
    // unit-test it. Full VkBuffer/Memory paths are validated by
    // hardware smoke in 3A T5.

    fn align_up(offset: u64, alignment: u64) -> u64 {
        (offset + alignment - 1) & !(alignment - 1)
    }

    #[test]
    fn align_up_pow2() {
        assert_eq!(align_up(0, 16), 0);
        assert_eq!(align_up(1, 16), 16);
        assert_eq!(align_up(16, 16), 16);
        assert_eq!(align_up(17, 16), 32);
        assert_eq!(align_up(1023, 256), 1024);
    }

    #[test]
    fn fits_in_chunk() {
        // (used, alignment, size, chunk_size) → would_fit
        let cases = [
            (0u64, 16u64, 100u64, 1024u64, true),
            (900, 16, 100, 1024, true),  // 912 + 100 = 1012 ≤ 1024
            (912, 16, 200, 1024, false), // 912 + 200 = 1112 > 1024
            (0, 256, 1024, 1024, true),
        ];
        for (used, align, size, chunk_size, expected) in cases {
            let aligned = align_up(used, align);
            let fits = aligned + size <= chunk_size;
            assert_eq!(fits, expected, "used={used} align={align} size={size}");
        }
    }
}
