//! Implicit dma-buf fence bridge via the `DMA_BUF_IOCTL_*_SYNC_FILE`
//! ioctls (`include/uapi/linux/dma-buf.h`).
//!
//! Vulkan does NOT participate in a dma-buf's implicit-sync
//! `reservation_object` automatically. A Vulkan *consumer* that
//! imports a client dma-buf (DRI3) and reads it must bridge the
//! producer's implicit fence into an explicit `VkSemaphore` wait:
//!
//! 1. `EXPORT_SYNC_FILE(READ)` extracts the buffer's current implicit
//!    fence as a `sync_file` fd ([`export_read_sync_file`]).
//! 2. `kms::vk::sync::import_sync_file` imports it as a binary
//!    `VkSemaphore`.
//! 3. The copy/compose `vkQueueSubmit2` waits on that semaphore.
//!
//! Without this, `backend.copy_area` reads the imported dma-buf while
//! the producer's GPU may still be writing → intermittent blank/torn
//! frames (the Firefox HW-WebRender bug; see
//! `docs/plans/2026-05-29-dri3-implicit-dmabuf-sync.md`).
//!
//! The fence advances per producer GPU write, so the EXPORT must be
//! done **fresh at each present/copy**, never cached at import.
//!
//! IOCTL style mirrors `kms::console` (raw `libc::ioctl` + manually
//! derived request constants) — the tree has no generic ioctl crate.

use std::{
    io,
    os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd},
};

/// `struct dma_buf_sync_file` from `include/uapi/linux/dma-buf.h`:
/// ```c
/// struct dma_buf_sync_file {
///     __u32 flags;
///     __s32 fd;
/// };
/// ```
#[repr(C)]
struct DmaBufSyncFile {
    flags: u32,
    fd: i32,
}

/// `DMA_BUF_SYNC_READ` — export the fence that gates *readers* (i.e.
/// the producer's outstanding writes that a reader must wait for).
const DMA_BUF_SYNC_READ: u32 = 1 << 0;

/// `DMA_BUF_IOCTL_EXPORT_SYNC_FILE = _IOWR('b', 2, struct dma_buf_sync_file)`.
///
/// Derivation (asm-generic `_IOC`): dir = `_IOC_READ | _IOC_WRITE` = 3,
/// size = `size_of::<dma_buf_sync_file>()` = 8, type = `'b'` = 0x62,
/// nr = 2 →
/// `(3 << 30) | (8 << 16) | (0x62 << 8) | 2` = `0xC008_6202`.
const DMA_BUF_IOCTL_EXPORT_SYNC_FILE: libc::c_ulong = 0xC008_6202;

/// Export the dma-buf's current implicit READ fence as a `sync_file`.
///
/// Returns:
/// - `Ok(Some(fd))` — the buffer has an outstanding producer fence; the
///   caller imports `fd` as a `VkSemaphore` and waits on it.
/// - `Ok(None)` — the buffer is idle (kernel returned an invalid fd);
///   nothing to wait on.
/// - `Err(_)` — the ioctl itself failed.
///
/// # Errors
/// Propagates the underlying `ioctl(2)` error (e.g. `ENOTTY` on a
/// kernel without `DMA_BUF_IOCTL_EXPORT_SYNC_FILE`, or `EINVAL`).
pub fn export_read_sync_file(dmabuf_fd: BorrowedFd<'_>) -> io::Result<Option<OwnedFd>> {
    let mut arg = DmaBufSyncFile {
        flags: DMA_BUF_SYNC_READ,
        // -1 sentinel: the kernel overwrites this with the exported fd
        // on success, or leaves it < 0 if the buffer has no fence.
        fd: -1,
    };
    // SAFETY: `dmabuf_fd` is a valid borrowed fd; `arg` is a live,
    // correctly-shaped `struct dma_buf_sync_file` on the stack that
    // the kernel reads (flags) and writes (fd).
    let rc = unsafe {
        libc::ioctl(
            dmabuf_fd.as_raw_fd(),
            DMA_BUF_IOCTL_EXPORT_SYNC_FILE,
            std::ptr::from_mut(&mut arg),
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    let raw: RawFd = arg.fd;
    if raw < 0 {
        // Buffer idle — no producer fence to wait on.
        return Ok(None);
    }
    // SAFETY: the ioctl succeeded and returned a fresh, owned fd.
    Ok(Some(unsafe { OwnedFd::from_raw_fd(raw) }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_sync_file_request_code_matches_uapi() {
        // _IOWR('b', 2, struct dma_buf_sync_file), 8-byte struct.
        assert_eq!(DMA_BUF_IOCTL_EXPORT_SYNC_FILE, 0xC008_6202);
    }

    #[test]
    fn sync_file_struct_is_eight_bytes() {
        // The request-code size field (8) is derived from this; if the
        // struct ever changes shape the ioctl number above is wrong.
        assert_eq!(std::mem::size_of::<DmaBufSyncFile>(), 8);
    }

    #[test]
    fn read_flag_is_bit_zero() {
        assert_eq!(DMA_BUF_SYNC_READ, 1);
    }
}
