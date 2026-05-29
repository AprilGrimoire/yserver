//! `DrawableImage` — per-X-resource `VkImage` wrapper for the
//! Vulkan-side scene graph (sub-phase 4.1.3).
//!
//! Spec: docs/superpowers/specs/2026-05-07-phase4-1-vulkan-compositor-design.md
//! §"Per X resource — DrawableImage abstraction".
//!
//! Every X drawable backed by GPU memory (windows + pixmaps) holds a
//! `DrawableImage` alongside its pixman image. Drawing ops still
//! route through pixman in 4.1.3; the result uploads to the
//! corresponding `DrawableImage` on damage. The composite pass reads
//! per-window mirrors instead of a unified shadow buffer (which
//! eliminates the whole-framebuffer memcpy that PixmanShadow incurs
//! today).
//!
//! Two backing variants are defined from day one. Only `ServerOwned`
//! is exercised by 4.1.3-4.1.4 (server-allocated CreateWindow /
//! CreatePixmap mirrors). `Imported` is the slot for Phase 4.2 (DRI3
//! / Present) where clients hand us a dma-buf to scan out — having
//! the variant in the enum from the start means 4.2 doesn't need to
//! retrofit a new constructor onto a sealed type.

use std::sync::Arc;

use ash::vk;

use super::device::VkContext;

/// Per-drawable Vulkan image. Lives as long as the X drawable does;
/// dropped when the client calls `DestroyWindow` / `FreePixmap`.
pub struct DrawableImage {
    pub vk_image: vk::Image,
    /// Color image view bound by the composite pass's descriptor
    /// set. One view per drawable, lifetime matches `vk_image`.
    pub vk_image_view: vk::ImageView,
    /// Lazy view used when this drawable is bound as a *mask* in
    /// the 4.1.4.6 RENDER pipeline. For `B8G8R8A8_UNORM` mirrors
    /// the regular view's `.a` already returns alpha; we use the
    /// same handle. For `R8_UNORM` mirrors we lazily build a
    /// view with component swizzle `(R,R,R,R)` so the shader's
    /// uniform `mask.a` read still returns the alpha-bearing
    /// `R` channel. `None` until the drawable is first sampled
    /// as a mask.
    mask_view: Option<vk::ImageView>,
    /// Lazy view used when this drawable is bound as a *source*
    /// for a picture format with no alpha mask (depth-24 r8g8b8 /
    /// x8r8g8b8 pictures). Component swizzle `(R, G, B, ONE)`,
    /// matching X RENDER's "alpha defaults to 1 when missing".
    /// Built lazily on first such use; only meaningful for BGRA
    /// mirrors. `None` until requested.
    no_alpha_src_view: Option<vk::ImageView>,
    pub extent: vk::Extent2D,
    pub format: vk::Format,
    pub backing: ImageBacking,
    /// Damage accumulated since the last successful upload. Updated
    /// at every drawing-op call site (Task 3.3 marks whole-image
    /// damage; 4.1.4 family ports tighten to per-op rects).
    damage: MirrorDamage,
    /// Layout the image is currently in. Image is created `UNDEFINED`;
    /// after the first damage upload we transition through
    /// `TRANSFER_DST_OPTIMAL` and leave it in `SHADER_READ_ONLY_OPTIMAL`
    /// for the eventual composite-pass read (4.1.3.4 wires the read).
    /// The next upload then transitions back through
    /// `TRANSFER_DST_OPTIMAL`. We track the current layout so the
    /// uploader picks the right `oldLayout`.
    current_layout: vk::ImageLayout,
    /// Held for Drop so the Vulkan handles can be destroyed against
    /// a live device.
    vk: Arc<VkContext>,
}

/// Damage accumulator on the mirror. Tracks either "the whole image
/// is dirty" (set by drawing ops that don't have a tight rect — most
/// of them, in 4.1.3) or a bounding box that grows as rects come in.
/// 4.1.4's per-op family ports replace `mark_full` calls with
/// `mark_rect` calls for the cases where the op knows its damage.
#[derive(Debug, Default, Clone, Copy)]
pub struct MirrorDamage {
    /// `true` when at least one drawing op marked damage without a
    /// rect (or when `mark_full` was called explicitly). Suppresses
    /// per-rect bbox tracking for the rest of the frame; the upload
    /// will copy the whole image.
    fully_dirty: bool,
    /// Bounding box of all marked rects (only meaningful when
    /// `fully_dirty == false` and `bbox != None`).
    bbox: Option<vk::Rect2D>,
}

impl MirrorDamage {
    /// Mark every pixel in the image as needing re-upload. Coarsest
    /// granularity; the safe default for any op that mutates the
    /// pixman image without a known damage rect.
    pub fn mark_full(&mut self) {
        self.fully_dirty = true;
    }

    /// Mark a single rect as dirty, growing the bounding box. No-op
    /// when `fully_dirty` is already set.
    #[allow(dead_code)] // wired in by 4.1.4 per-op family ports.
    pub fn mark_rect(&mut self, rect: vk::Rect2D) {
        if self.fully_dirty || rect.extent.width == 0 || rect.extent.height == 0 {
            return;
        }
        self.bbox = Some(match self.bbox {
            None => rect,
            Some(prev) => union_rect(prev, rect),
        });
    }

    /// Take the accumulated damage as a single rect to upload, if
    /// any. Returns the bbox clamped to `[0..extent.width] ×
    /// [0..extent.height]`, or `None` when nothing was marked.
    /// Clears the accumulator.
    pub fn take(&mut self, extent: vk::Extent2D) -> Option<vk::Rect2D> {
        let was_full = std::mem::take(&mut self.fully_dirty);
        let bbox = self.bbox.take();
        if was_full {
            return Some(vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 0 },
                extent,
            });
        }
        let rect = bbox?;
        Some(clamp_rect_to_extent(rect, extent))
    }

    /// Whether anything is dirty.
    #[allow(dead_code)] // exposed for callers that may want to skip uploads early.
    pub fn is_dirty(&self) -> bool {
        self.fully_dirty || self.bbox.is_some()
    }
}

fn union_rect(a: vk::Rect2D, b: vk::Rect2D) -> vk::Rect2D {
    let ax0 = a.offset.x;
    let ay0 = a.offset.y;
    let ax1 = a.offset.x.saturating_add_unsigned(a.extent.width);
    let ay1 = a.offset.y.saturating_add_unsigned(a.extent.height);
    let bx0 = b.offset.x;
    let by0 = b.offset.y;
    let bx1 = b.offset.x.saturating_add_unsigned(b.extent.width);
    let by1 = b.offset.y.saturating_add_unsigned(b.extent.height);
    let x0 = ax0.min(bx0);
    let y0 = ay0.min(by0);
    let x1 = ax1.max(bx1);
    let y1 = ay1.max(by1);
    vk::Rect2D {
        offset: vk::Offset2D { x: x0, y: y0 },
        extent: vk::Extent2D {
            width: u32::try_from((x1 - x0).max(0)).unwrap_or(0),
            height: u32::try_from((y1 - y0).max(0)).unwrap_or(0),
        },
    }
}

fn clamp_rect_to_extent(rect: vk::Rect2D, extent: vk::Extent2D) -> vk::Rect2D {
    let x0 = rect.offset.x.max(0);
    let y0 = rect.offset.y.max(0);
    let max_x = i32::try_from(extent.width).unwrap_or(i32::MAX);
    let max_y = i32::try_from(extent.height).unwrap_or(i32::MAX);
    let x1 = rect
        .offset
        .x
        .saturating_add_unsigned(rect.extent.width)
        .clamp(0, max_x);
    let y1 = rect
        .offset
        .y
        .saturating_add_unsigned(rect.extent.height)
        .clamp(0, max_y);
    vk::Rect2D {
        offset: vk::Offset2D { x: x0, y: y0 },
        extent: vk::Extent2D {
            width: u32::try_from((x1 - x0).max(0)).unwrap_or(0),
            height: u32::try_from((y1 - y0).max(0)).unwrap_or(0),
        },
    }
}

/// What's behind the `VkImage`.
#[allow(dead_code)] // Imported variant exercised by Phase 4.2 (DRI3).
pub enum ImageBacking {
    /// We allocated the memory ourselves. This is the case for every
    /// CreateWindow / CreatePixmap mirror through Phase 4.1.5.
    ServerOwned { vk_memory: vk::DeviceMemory },
    /// Memory was imported from a client-supplied dma-buf (DRI3).
    /// Phase 4.2 territory; the field shape is fixed now so 4.2's
    /// constructor can drop into the existing enum without the
    /// invasive refactor of a sealed type.
    Imported {
        dma_buf_fd: std::os::fd::OwnedFd,
        vk_memory: vk::DeviceMemory,
    },
}

/// Errors during `DrawableImage` creation.
#[derive(Debug, thiserror::Error)]
#[allow(dead_code)] // raised by Task 3.2 once mirrors are allocated per-window.
pub enum DrawableImageError {
    #[error("vulkan: {0}")]
    Vk(vk::Result),
    #[error("no memory type matches image requirements")]
    NoMemoryType,
}

impl From<vk::Result> for DrawableImageError {
    fn from(r: vk::Result) -> Self {
        DrawableImageError::Vk(r)
    }
}

impl DrawableImage {
    /// Allocate a server-owned mirror for an X window. Format is
    /// fixed `B8G8R8A8_UNORM` (matches the rest of the
    /// Phase 4.1 surface palette and pixman's `X8R8G8B8` shadow
    /// layout). Memory is bound directly (one device allocation per
    /// drawable for now; a pool allocator can amortize later if
    /// session window counts ever push us toward
    /// `maxMemoryAllocationCount`).
    #[allow(dead_code)] // called from Task 3.2 (per-window mirror creation).
    pub fn new_server_owned_window(
        vk: Arc<VkContext>,
        width: u32,
        height: u32,
    ) -> Result<Self, DrawableImageError> {
        Self::new_server_owned(vk, width, height, vk::Format::B8G8R8A8_UNORM)
    }

    /// Map an X11 pixmap depth to its server-owned mirror format.
    /// Used by both [`Self::new_server_owned_pixmap`] (for fresh
    /// allocation) and `KmsBackend::allocate_pixmap_mirror`'s pool-key
    /// derivation, so the two sites cannot drift.
    #[must_use]
    pub fn format_for_pixmap_depth(depth: u8) -> vk::Format {
        match depth {
            1 | 8 => vk::Format::R8_UNORM,
            24 | 32 => vk::Format::B8G8R8A8_UNORM,
            other => {
                log::warn!(
                    "DrawableImage::format_for_pixmap_depth: unhandled depth {other} → \
                     defaulting to B8G8R8A8_UNORM (4.1.4 should fix the format-mapping table)"
                );
                vk::Format::B8G8R8A8_UNORM
            }
        }
    }

    /// Allocate a server-owned mirror for an X pixmap. Format is
    /// derived from the pixmap's depth (only depths 1, 8, 24, 32 are
    /// in scope today, matching pixman's `FormatCode` set).
    #[allow(dead_code)] // called from Task 3.2 (per-pixmap mirror creation).
    pub fn new_server_owned_pixmap(
        vk: Arc<VkContext>,
        width: u32,
        height: u32,
        depth: u8,
    ) -> Result<Self, DrawableImageError> {
        let format = Self::format_for_pixmap_depth(depth);
        Self::new_server_owned(vk, width, height, format)
    }

    /// Import a client-supplied dma-buf (DRI3) as the backing of a
    /// drawable. Phase 4.2 design §3.2.
    ///
    /// **fd ownership rule.** This function takes ownership of
    /// `dma_buf_fd` and transfers it to the resulting `VkDeviceMemory`
    /// on success. On any error path before `vkAllocateMemory` returns
    /// `SUCCESS`, the fd is closed (via the `OwnedFd` drop). On
    /// success, fd lifetime is owned by `VkDeviceMemory` —
    /// `Drop for DrawableImage` calls `vkFreeMemory` which releases it.
    pub fn from_dmabuf(
        vk: Arc<VkContext>,
        dma_buf_fd: std::os::fd::OwnedFd,
        width: u32,
        height: u32,
        format: vk::Format,
        modifier: u64,
        plane_offsets: &[u64],
        plane_pitches: &[u32],
    ) -> Result<Self, DrawableImageError> {
        use std::os::fd::IntoRawFd as _;
        if plane_offsets.len() != plane_pitches.len() {
            return Err(DrawableImageError::Vk(
                vk::Result::ERROR_INITIALIZATION_FAILED,
            ));
        }
        if plane_offsets.is_empty() || plane_offsets.len() > 4 {
            return Err(DrawableImageError::Vk(
                vk::Result::ERROR_INITIALIZATION_FAILED,
            ));
        }

        // Use the explicit-modifier path whenever the extension is
        // available — including LINEAR. VK_IMAGE_TILING_LINEAR alone
        // makes the driver compute its own row pitch from format/width
        // and ignores the client-supplied stride, which silently
        // corrupts imports whose stride doesn't match the driver's
        // alignment (e.g. Mesa anv allocates 300×BGRA8888 with
        // stride=1280, not the 1200 a tight LINEAR layout would use).
        let use_explicit_modifier = vk.image_drm_format_modifier;

        // Build VkSubresourceLayout array for the explicit-modifier chain
        // (only used when use_explicit_modifier is true).
        let plane_layouts: Vec<vk::SubresourceLayout> = plane_offsets
            .iter()
            .zip(plane_pitches.iter())
            .map(|(&offset, &pitch)| vk::SubresourceLayout {
                offset,
                size: 0,
                row_pitch: u64::from(pitch),
                array_pitch: 0,
                depth_pitch: 0,
            })
            .collect();
        let mut modifier_info = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
            .drm_format_modifier(modifier)
            .plane_layouts(&plane_layouts);

        let mut external_info = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        if use_explicit_modifier {
            external_info.p_next =
                std::ptr::from_mut(&mut modifier_info).cast::<std::ffi::c_void>();
        }

        let tiling = if use_explicit_modifier {
            vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT
        } else {
            vk::ImageTiling::LINEAR
        };

        let image_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(tiling)
            .usage(
                vk::ImageUsageFlags::SAMPLED
                    | vk::ImageUsageFlags::TRANSFER_SRC
                    | vk::ImageUsageFlags::TRANSFER_DST
                    | vk::ImageUsageFlags::COLOR_ATTACHMENT,
            )
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut external_info);

        let image = unsafe { vk.device.create_image(&image_info, None)? };

        // Per design §3.2 fd ownership rule: dup the fd before handing
        // to vkAllocateMemory so that on any vkAllocateMemory failure
        // we can close ours and let the caller's `dma_buf_fd` drop
        // separately. On vkAllocateMemory SUCCESS the dup'd fd is
        // owned by the VkDeviceMemory; `into_raw_fd` releases the
        // OwnedFd's drop responsibility.
        let server_fd_owned = dma_buf_fd.try_clone().map_err(|_| {
            unsafe { vk.device.destroy_image(image, None) };
            DrawableImageError::Vk(vk::Result::ERROR_OUT_OF_HOST_MEMORY)
        })?;
        let server_fd_raw = server_fd_owned.into_raw_fd();

        let mut import_info = vk::ImportMemoryFdInfoKHR::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
            .fd(server_fd_raw);

        let mem_reqs = unsafe { vk.device.get_image_memory_requirements(image) };
        let mem_props = unsafe {
            vk.instance
                .get_physical_device_memory_properties(vk.physical_device)
        };
        let memory_type_index = pick_memory_type(
            &mem_props,
            mem_reqs.memory_type_bits,
            vk::MemoryPropertyFlags::empty(),
        );
        let memory_type_index = match memory_type_index {
            Some(i) => i,
            None => {
                unsafe {
                    vk.device.destroy_image(image, None);
                    libc::close(server_fd_raw);
                }
                return Err(DrawableImageError::NoMemoryType);
            }
        };
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let alloc_info = vk::MemoryAllocateInfo::default()
            .allocation_size(mem_reqs.size)
            .memory_type_index(memory_type_index)
            .push_next(&mut import_info)
            .push_next(&mut dedicated);
        let memory = match unsafe { vk.device.allocate_memory(&alloc_info, None) } {
            Ok(m) => m,
            Err(e) => {
                unsafe {
                    vk.device.destroy_image(image, None);
                    // vkAllocateMemory consumed the fd only on SUCCESS.
                    libc::close(server_fd_raw);
                }
                return Err(e.into());
            }
        };
        // server_fd_raw is now owned by `memory`. Caller's
        // `dma_buf_fd` is still owned by the OwnedFd we received; it
        // will be re-stashed on the Imported variant so the original
        // fd survives at least until DrawableImage drop.
        if let Err(e) = unsafe { vk.device.bind_image_memory(image, memory, 0) } {
            unsafe {
                vk.device.free_memory(memory, None);
                vk.device.destroy_image(image, None);
            }
            return Err(e.into());
        }

        let view_info = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(format)
            .subresource_range(
                vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .level_count(1)
                    .layer_count(1),
            );
        let view = match unsafe { vk.device.create_image_view(&view_info, None) } {
            Ok(v) => v,
            Err(e) => {
                unsafe {
                    vk.device.free_memory(memory, None);
                    vk.device.destroy_image(image, None);
                }
                return Err(e.into());
            }
        };

        Ok(Self {
            vk_image: image,
            vk_image_view: view,
            mask_view: None,
            no_alpha_src_view: None,
            extent: vk::Extent2D { width, height },
            format,
            backing: ImageBacking::Imported {
                dma_buf_fd,
                vk_memory: memory,
            },
            damage: MirrorDamage::default(),
            current_layout: vk::ImageLayout::UNDEFINED,
            vk,
        })
    }

    fn new_server_owned(
        vk: Arc<VkContext>,
        width: u32,
        height: u32,
        format: vk::Format,
    ) -> Result<Self, DrawableImageError> {
        let image_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(
                vk::ImageUsageFlags::COLOR_ATTACHMENT
                    | vk::ImageUsageFlags::TRANSFER_DST
                    | vk::ImageUsageFlags::TRANSFER_SRC
                    | vk::ImageUsageFlags::SAMPLED,
            )
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        let image = unsafe { vk.device.create_image(&image_info, None)? };

        let mem_reqs = unsafe { vk.device.get_image_memory_requirements(image) };
        let mem_props = unsafe {
            vk.instance
                .get_physical_device_memory_properties(vk.physical_device)
        };
        let memory_type_index = pick_memory_type(
            &mem_props,
            mem_reqs.memory_type_bits,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
        )
        .ok_or(DrawableImageError::NoMemoryType);
        let memory_type_index = match memory_type_index {
            Ok(i) => i,
            Err(e) => {
                unsafe { vk.device.destroy_image(image, None) };
                return Err(e);
            }
        };
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let alloc_info = vk::MemoryAllocateInfo::default()
            .allocation_size(mem_reqs.size)
            .memory_type_index(memory_type_index)
            .push_next(&mut dedicated);
        let memory = match unsafe { vk.device.allocate_memory(&alloc_info, None) } {
            Ok(m) => m,
            Err(e) => {
                unsafe { vk.device.destroy_image(image, None) };
                return Err(e.into());
            }
        };
        if let Err(e) = unsafe { vk.device.bind_image_memory(image, memory, 0) } {
            unsafe {
                vk.device.free_memory(memory, None);
                vk.device.destroy_image(image, None);
            }
            return Err(e.into());
        }

        let view_info = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(format)
            .subresource_range(
                vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .level_count(1)
                    .layer_count(1),
            );
        let view = match unsafe { vk.device.create_image_view(&view_info, None) } {
            Ok(v) => v,
            Err(e) => {
                unsafe {
                    vk.device.free_memory(memory, None);
                    vk.device.destroy_image(image, None);
                }
                return Err(e.into());
            }
        };

        Ok(Self {
            vk_image: image,
            vk_image_view: view,
            mask_view: None,
            no_alpha_src_view: None,
            extent: vk::Extent2D { width, height },
            format,
            backing: ImageBacking::ServerOwned { vk_memory: memory },
            damage: MirrorDamage::default(),
            current_layout: vk::ImageLayout::UNDEFINED,
            vk,
        })
    }

    /// Backing `VkDeviceMemory` handle. Used by the DRI3 export path
    /// (Task 13) to call `vkGetMemoryFdKHR` against either the
    /// server-allocated memory (ServerOwned) or the previously
    /// imported memory (Imported). Both variants carry the field;
    /// callers don't need to know which.
    pub fn backing_memory(&self) -> vk::DeviceMemory {
        match &self.backing {
            ImageBacking::ServerOwned { vk_memory } => *vk_memory,
            ImageBacking::Imported { vk_memory, .. } => *vk_memory,
        }
    }

    /// Borrow the retained dma-buf fd for an `Imported` backing, so the
    /// implicit-sync bridge can `EXPORT_SYNC_FILE` against it per-copy
    /// (see `kms::vk::dmabuf_sync`). `None` for `ServerOwned` backings
    /// (no client dma-buf, hence no implicit producer fence to honor).
    /// The fd's lifetime is owned by the `Imported` variant and is
    /// closed on `DrawableImage::drop`.
    #[must_use]
    pub fn dma_buf_fd(&self) -> Option<std::os::fd::BorrowedFd<'_>> {
        use std::os::fd::AsFd as _;
        match &self.backing {
            ImageBacking::ServerOwned { .. } => None,
            ImageBacking::Imported { dma_buf_fd, .. } => Some(dma_buf_fd.as_fd()),
        }
    }

    /// Image view to bind when this BGRA mirror is sampled as a
    /// *source* for a picture format with no alpha mask (depth-24
    /// r8g8b8 / x8r8g8b8 pictures). The X RENDER spec says missing
    /// alpha defaults to 1; the swizzle `(R, G, B, ONE)` enforces
    /// that at sample time without per-pipeline shader changes.
    /// For `R8_UNORM` mirrors callers should use [`mask_image_view`]
    /// instead — R8 is alpha-only by definition.
    pub fn no_alpha_src_image_view(&mut self) -> Result<vk::ImageView, vk::Result> {
        if let Some(v) = self.no_alpha_src_view {
            return Ok(v);
        }
        let view_info = vk::ImageViewCreateInfo::default()
            .image(self.vk_image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(self.format)
            .components(vk::ComponentMapping {
                r: vk::ComponentSwizzle::IDENTITY,
                g: vk::ComponentSwizzle::IDENTITY,
                b: vk::ComponentSwizzle::IDENTITY,
                a: vk::ComponentSwizzle::ONE,
            })
            .subresource_range(
                vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .level_count(1)
                    .layer_count(1),
            );
        let view = unsafe { self.vk.device.create_image_view(&view_info, None)? };
        self.no_alpha_src_view = Some(view);
        Ok(view)
    }

    /// Image view to bind when this mirror is sampled as a *mask*
    /// in the 4.1.4.6 RENDER pipeline. For `B8G8R8A8_UNORM` mirrors
    /// the regular view's `.a` is already alpha; we return the same
    /// handle. For `R8_UNORM` mirrors we lazily create a swizzled
    /// view (`a = R`) so the shader can uniformly read `mask.a`
    /// without knowing the underlying format. Built once per
    /// mirror; reused for every Composite that uses it as mask.
    pub fn mask_image_view(&mut self) -> Result<vk::ImageView, vk::Result> {
        if self.format == vk::Format::B8G8R8A8_UNORM {
            return Ok(self.vk_image_view);
        }
        if let Some(v) = self.mask_view {
            return Ok(v);
        }
        let view_info = vk::ImageViewCreateInfo::default()
            .image(self.vk_image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(self.format)
            .components(vk::ComponentMapping {
                r: vk::ComponentSwizzle::ZERO,
                g: vk::ComponentSwizzle::ZERO,
                b: vk::ComponentSwizzle::ZERO,
                a: vk::ComponentSwizzle::R,
            })
            .subresource_range(
                vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .level_count(1)
                    .layer_count(1),
            );
        let view = unsafe { self.vk.device.create_image_view(&view_info, None)? };
        self.mask_view = Some(view);
        Ok(view)
    }

    /// Mark every pixel as dirty. Called at every pixman drawing-op
    /// site that mutates this drawable (Task 3.3 shim — 4.1.4 per-op
    /// ports replace this with rect-precise [`Self::mark_damage_rect`]
    /// where they can).
    pub fn mark_full_damage(&mut self) {
        self.damage.mark_full();
    }

    /// Mark a sub-rect as dirty.
    #[allow(dead_code)] // wired in by 4.1.4 per-op family ports.
    pub fn mark_damage_rect(&mut self, rect: vk::Rect2D) {
        self.damage.mark_rect(rect);
    }

    /// Pop the accumulated damage as a rect to upload, or `None` if
    /// the mirror is clean. Clears the accumulator.
    pub fn take_damage(&mut self) -> Option<vk::Rect2D> {
        self.damage.take(self.extent)
    }

    /// Whether the mirror has unflushed pixman-side writes.
    /// Currently only used internally; the 4.1.4.6 RENDER path
    /// no longer gates on it (see backend.rs `ensure_drawable_mirror_sampleable`).
    pub fn is_dirty(&self) -> bool {
        self.damage.is_dirty()
    }

    /// Run a one-shot CB that clears the freshly-created mirror to
    /// `(0, 0, 0, 0)` and transitions it to
    /// `SHADER_READ_ONLY_OPTIMAL`. Called by `KmsBackend` right
    /// after each mirror is constructed so the very first
    /// `RenderComposite` sample sees defined contents (zeros)
    /// instead of UB.
    ///
    /// `pool` is the backend's drawing-op command pool.
    pub fn initialize_clear(&mut self, pool: vk::CommandPool) -> Result<(), vk::Result> {
        let alloc_info = vk::CommandBufferAllocateInfo::default()
            .command_pool(pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        let cb = unsafe { self.vk.device.allocate_command_buffers(&alloc_info)?[0] };

        let result = (|| -> Result<(), vk::Result> {
            let device = &self.vk.device;
            let begin = vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
            crate::vk_count!(begin_command_buffer);
            unsafe { device.begin_command_buffer(cb, &begin)? };

            let to_dst = [vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                .src_access_mask(vk::AccessFlags2::empty())
                .dst_stage_mask(vk::PipelineStageFlags2::CLEAR)
                .dst_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .image(self.vk_image)
                .subresource_range(
                    vk::ImageSubresourceRange::default()
                        .aspect_mask(vk::ImageAspectFlags::COLOR)
                        .level_count(1)
                        .layer_count(1),
                )];
            let dep = vk::DependencyInfo::default().image_memory_barriers(&to_dst);
            crate::vk_count!(cmd_pipeline_barrier2);
            unsafe { device.cmd_pipeline_barrier2(cb, &dep) };

            let clear_color = vk::ClearColorValue {
                float32: [0.0, 0.0, 0.0, 0.0],
            };
            let ranges = [vk::ImageSubresourceRange::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .level_count(1)
                .layer_count(1)];
            unsafe {
                crate::vk_count!(cmd_clear_color_image);
                device.cmd_clear_color_image(
                    cb,
                    self.vk_image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &clear_color,
                    &ranges,
                );
            }

            let to_read = [vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::CLEAR)
                .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                .dst_stage_mask(vk::PipelineStageFlags2::FRAGMENT_SHADER)
                .dst_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
                .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .image(self.vk_image)
                .subresource_range(
                    vk::ImageSubresourceRange::default()
                        .aspect_mask(vk::ImageAspectFlags::COLOR)
                        .level_count(1)
                        .layer_count(1),
                )];
            let dep = vk::DependencyInfo::default().image_memory_barriers(&to_read);
            crate::vk_count!(cmd_pipeline_barrier2);
            unsafe { device.cmd_pipeline_barrier2(cb, &dep) };

            crate::vk_count!(end_command_buffer);
            unsafe { device.end_command_buffer(cb)? };

            let cb_info = [vk::CommandBufferSubmitInfo::default().command_buffer(cb)];
            let submit = [vk::SubmitInfo2::default().command_buffer_infos(&cb_info)];
            unsafe {
                crate::vk_count!(queue_submit2);
                crate::vk_count!(submit_other);
                device.queue_submit2(self.vk.graphics_queue, &submit, vk::Fence::null())?;
                device.queue_wait_idle(self.vk.graphics_queue)?;
            }
            Ok(())
        })();

        unsafe { self.vk.device.free_command_buffers(pool, &[cb]) };
        if result.is_ok() {
            self.current_layout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
        }
        result
    }

    /// Current layout of the image. Bumped by upload paths after
    /// they land a transition.
    pub fn current_layout(&self) -> vk::ImageLayout {
        self.current_layout
    }

    /// Set the tracked layout. The upload code path calls this after
    /// recording the matching `vkCmdPipelineBarrier2`.
    pub fn set_current_layout(&mut self, layout: vk::ImageLayout) {
        self.current_layout = layout;
    }

    /// Record into `cb` the barriers + copy that upload `rect` from a
    /// host-visible staging buffer into this image. Caller has
    /// already memcpy'd the rect's pixel rows into staging starting
    /// at `staging_offset_bytes`. Buffer rows in staging are tightly
    /// packed (no row padding); `rect.extent.width × bpp` bytes per
    /// row.
    ///
    /// The final layout is `SHADER_READ_ONLY_OPTIMAL` — the layout
    /// the composite pass (4.1.3.4) will sample from.
    pub fn record_upload_rect(
        &mut self,
        cb: vk::CommandBuffer,
        staging_buffer: vk::Buffer,
        staging_offset_bytes: u64,
        rect: vk::Rect2D,
    ) {
        let device = &self.vk.device;
        let old_layout = self.current_layout;

        // Transition into TRANSFER_DST_OPTIMAL.
        let to_dst = vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
            .src_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
            .dst_stage_mask(vk::PipelineStageFlags2::COPY)
            .dst_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
            .old_layout(old_layout)
            .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .image(self.vk_image)
            .subresource_range(
                vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .level_count(1)
                    .layer_count(1),
            );
        let to_dst_arr = [to_dst];
        let to_dst_dep = vk::DependencyInfo::default().image_memory_barriers(&to_dst_arr);
        crate::vk_count!(cmd_pipeline_barrier2);
        unsafe { device.cmd_pipeline_barrier2(cb, &to_dst_dep) };

        // Copy staging buffer rect → image.
        let copy_region = vk::BufferImageCopy::default()
            .buffer_offset(staging_offset_bytes)
            .buffer_row_length(0) // tightly packed in staging
            .buffer_image_height(0)
            .image_subresource(
                vk::ImageSubresourceLayers::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .layer_count(1),
            )
            .image_offset(vk::Offset3D {
                x: rect.offset.x,
                y: rect.offset.y,
                z: 0,
            })
            .image_extent(vk::Extent3D {
                width: rect.extent.width,
                height: rect.extent.height,
                depth: 1,
            });
        let regions = [copy_region];
        unsafe {
            crate::vk_count!(cmd_copy_buffer_to_image);
            device.cmd_copy_buffer_to_image(
                cb,
                staging_buffer,
                self.vk_image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &regions,
            );
        }

        // Transition into SHADER_READ_ONLY_OPTIMAL — the layout the
        // 4.1.3.4 composite pass will sample.
        let to_read = vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::COPY)
            .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
            .dst_stage_mask(vk::PipelineStageFlags2::FRAGMENT_SHADER)
            .dst_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
            .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .image(self.vk_image)
            .subresource_range(
                vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .level_count(1)
                    .layer_count(1),
            );
        let to_read_arr = [to_read];
        let to_read_dep = vk::DependencyInfo::default().image_memory_barriers(&to_read_arr);
        crate::vk_count!(cmd_pipeline_barrier2);
        unsafe { device.cmd_pipeline_barrier2(cb, &to_read_dep) };

        self.current_layout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
    }

    /// Construct a `DrawableImage` from a pooled entry. The pool
    /// entry's `current_layout` is preserved so the first upload's
    /// pre-barrier transitions correctly. Caller is responsible for
    /// clearing the recycled image — X11 clients rely on the
    /// Xorg/Xephyr convention of zero-filled new pixmaps (composite
    /// of smaller-than-tile content onto an uncleared pool entry
    /// leaks the previous tenant's pixels).
    ///
    /// Lazy `mask_view` / `no_alpha_src_view` are set to `None`;
    /// they'll be rebuilt on demand if needed.
    pub fn new_from_pool(
        vk: Arc<VkContext>,
        entry: crate::kms::vk::pixmap_pool::PooledPixmapImage,
        format: vk::Format,
        extent: vk::Extent2D,
    ) -> Self {
        Self {
            vk_image: entry.image,
            vk_image_view: entry.view,
            mask_view: None,
            no_alpha_src_view: None,
            extent,
            format,
            backing: ImageBacking::ServerOwned {
                vk_memory: entry.memory,
            },
            damage: MirrorDamage::default(),
            current_layout: entry.current_layout,
            vk,
        }
    }

    /// Decompose a `DrawableImage` into a pooled-pixmap-shape
    /// (image, view, memory, current_layout) for return-to-pool.
    /// Destroys the lazy views first (they're format-specific and
    /// not pooled). Pool-bound Vulkan handles transfer to the
    /// returned `PooledPixmapImage`; the rest of `self` (notably
    /// the `Arc<VkContext>`) drops normally.
    ///
    /// **Important — does NOT use `mem::forget`.** Codex P1 from
    /// plan review round 1: forgetting `self` would leak the
    /// `Arc<VkContext>` strong-count, eventually preventing
    /// VkContext::Drop's device wait. Instead, the pool-bound
    /// handles are swapped with `vk::*::null()` so the normal
    /// `Drop for DrawableImage` runs but sees null handles —
    /// Vulkan's spec permits destroying null handles as a no-op
    /// (every `destroy_*` and `free_memory` call), so Drop becomes
    /// a no-op on the Vulkan side. The `Arc<VkContext>` and other
    /// non-handle fields drop normally.
    ///
    /// Panics if `backing` is `Imported` (DRI3 dma-buf imports
    /// don't go through the pool; caller must check).
    #[allow(dead_code)] // wired by pixmap-pool T2 (free_pixmap).
    pub fn into_pool_entry(mut self) -> crate::kms::vk::pixmap_pool::PooledPixmapImage {
        let ImageBacking::ServerOwned { vk_memory } = self.backing else {
            panic!("DrawableImage::into_pool_entry: Imported backing cannot be pooled");
        };
        // Destroy lazy format-specific views (not pooled).
        let mask_view = self.mask_view.take();
        let no_alpha = self.no_alpha_src_view.take();
        unsafe {
            if let Some(v) = mask_view {
                self.vk.device.destroy_image_view(v, None);
            }
            if let Some(v) = no_alpha {
                self.vk.device.destroy_image_view(v, None);
            }
        }
        // Swap pool-bound handles out, leaving nulls in their
        // place so self's Drop is a no-op for these handles.
        let image = std::mem::replace(&mut self.vk_image, vk::Image::null());
        let view = std::mem::replace(&mut self.vk_image_view, vk::ImageView::null());
        // Replace backing memory with null so Drop's free_memory
        // is also a no-op.
        self.backing = ImageBacking::ServerOwned {
            vk_memory: vk::DeviceMemory::null(),
        };

        // self drops here: Vk handle destruction calls are no-ops
        // on null; vk Arc drops normally; damage / layout fields
        // drop naturally.
        crate::kms::vk::pixmap_pool::PooledPixmapImage {
            image,
            view,
            memory: vk_memory,
            current_layout: self.current_layout,
        }
    }

    /// Bytes-per-pixel for the image's format. Only the formats this
    /// module actually allocates are recognised.
    pub fn bytes_per_pixel(&self) -> u32 {
        match self.format {
            vk::Format::R8_UNORM => 1,
            vk::Format::B8G8R8A8_UNORM => 4,
            // Should never happen: every constructor maps depth →
            // format from this set. If something else slipped through,
            // upstream is broken; assume 4 so the caller's stride math
            // doesn't divide by zero.
            other => {
                log::warn!(
                    "DrawableImage::bytes_per_pixel: unhandled format {other:?} → defaulting to 4"
                );
                4
            }
        }
    }
}

impl Drop for DrawableImage {
    fn drop(&mut self) {
        unsafe {
            // Image view must be destroyed before the image it
            // references; both must drop before the underlying
            // memory is freed.
            if let Some(v) = self.mask_view.take() {
                self.vk.device.destroy_image_view(v, None);
            }
            if let Some(v) = self.no_alpha_src_view.take() {
                self.vk.device.destroy_image_view(v, None);
            }
            self.vk.device.destroy_image_view(self.vk_image_view, None);
            self.vk.device.destroy_image(self.vk_image, None);
            match &self.backing {
                ImageBacking::ServerOwned { vk_memory }
                | ImageBacking::Imported { vk_memory, .. } => {
                    self.vk.device.free_memory(*vk_memory, None);
                }
            }
        }
        // dma-buf OwnedFd in Imported variant drops automatically.
    }
}

fn pick_memory_type(
    props: &vk::PhysicalDeviceMemoryProperties,
    type_bits: u32,
    required: vk::MemoryPropertyFlags,
) -> Option<u32> {
    (0..props.memory_type_count).find(|&i| {
        type_bits & (1 << i) != 0
            && props.memory_types[i as usize]
                .property_flags
                .contains(required)
    })
}

// Compile-only check that `from_dmabuf` keeps a stable signature
// across Phase 4.2 work. References every parameter type so an
// upstream rename in `ash` or `std::os::fd` breaks the build before
// 4.2 lands rather than at runtime.
#[cfg(test)]
#[allow(dead_code)]
fn _compile_check_from_dmabuf(
    vk: Arc<VkContext>,
    dma_buf_fd: std::os::fd::OwnedFd,
    modifier: u64,
    plane_offsets: &[u64],
    plane_pitches: &[u32],
) {
    let _ = DrawableImage::from_dmabuf(
        vk,
        dma_buf_fd,
        128,
        128,
        vk::Format::B8G8R8A8_UNORM,
        modifier,
        plane_offsets,
        plane_pitches,
    );
}
