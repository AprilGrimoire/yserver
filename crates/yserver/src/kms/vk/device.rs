//! VkContext: instance + physical/logical device + queues + debug messenger.
//!
//! Lifetime is the full backend lifetime. Drop order matters:
//! device-level handles before device, device before instance,
//! instance-level loaders before instance.

use ash::vk;
use std::{
    ffi::{CStr, c_char, c_void},
    sync::Arc,
};

use crate::platform::drm::DrmDeviceKey;

/// Kernel DRM-node identities reported by one Vulkan physical device through
/// `VK_EXT_physical_device_drm`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct VulkanDrmIdentity {
    pub(crate) primary: Option<DrmDeviceKey>,
    pub(crate) render: Option<DrmDeviceKey>,
}

/// Non-owning mapping from a DRM identity to a physical-device handle owned by
/// this context's Vulkan instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DrmDeviceSelection {
    primary: DrmDeviceKey,
    render: Option<DrmDeviceKey>,
}

/// Lives for the entire backend lifetime. Drop order matters: device
/// before instance; instance-level loaders before instance.
///
/// Extension loaders (`debug_utils_instance`, `external_semaphore_fd`)
/// must be stored, not reconstructed per call: the underlying ash
/// loader resolves function pointers via `vkGetInstanceProcAddr` /
/// `vkGetDeviceProcAddr` once and caches them. Drop also goes through
/// the loader (`destroy_debug_utils_messenger`).
#[allow(dead_code)] // fields populated incrementally across sub-phase 4.1.1.
pub struct VkContext {
    pub entry: ash::Entry,
    pub instance: ash::Instance,
    pub debug_utils_instance: ash::ext::debug_utils::Instance,
    pub physical_device: vk::PhysicalDevice,
    pub device: ash::Device,
    pub external_semaphore_fd: ash::khr::external_semaphore_fd::Device,
    pub external_memory_fd: Option<ash::khr::external_memory_fd::Device>,
    pub image_drm_format_modifier_ext: Option<ash::ext::image_drm_format_modifier::Device>,
    /// True when `VK_EXT_image_drm_format_modifier` is enabled on the
    /// device. Phase 4.2 DRI3 import needs this for non-LINEAR tilings;
    /// when false, `kms::vk::dri3::supported_modifiers` returns
    /// `[DRM_FORMAT_MOD_LINEAR]` per design §4 fallback matrix.
    pub image_drm_format_modifier: bool,
    /// GLX-TFP: per-driver tiling strategy for the exported image,
    /// cached on first successful allocation. LINEAR is preferred —
    /// Turnip / Adreno same-GPU dma-buf sharing only delivers live
    /// pixels through LINEAR (its modifier-tiled UBWC keeps
    /// compression metadata in driver caches that don't reach the
    /// dma-buf-backed memory, so the GL importer samples a frozen
    /// snapshot). RADV rejects LINEAR + COLOR_ATTACHMENT + dma-buf
    /// with `VK_ERROR_FORMAT_NOT_SUPPORTED`, in which case
    /// [`super::target::allocate_exportable`] falls back to the
    /// modifier path and caches that. Empty until the first
    /// allocation attempt.
    pub tfp_tiling_strategy: std::sync::OnceLock<super::target::TilingStrategy>,
    pub graphics_queue_family: u32,
    pub graphics_queue: vk::Queue,
    pub debug_messenger: Option<vk::DebugUtilsMessengerEXT>,
    /// Cached `VkPhysicalDeviceDriverProperties::driverID` for the
    /// picked device. Kept as a diagnostic for log lines / future
    /// driver-specific quirks; the scanout path itself no longer
    /// branches on it (the GBM-first cross-driver Venus problem
    /// went away with the Vulkan-first pivot).
    #[allow(dead_code)]
    pub driver_id: vk::DriverId,
    /// `VkPhysicalDeviceProperties::deviceType` of the picked device.
    /// `CPU` means a software rasterizer (llvmpipe/lavapipe) — usable
    /// for headless tests but NOT for real KMS scanout (see
    /// [`Self::is_software_rasterizer`]).
    pub device_type: vk::PhysicalDeviceType,
    /// Nanoseconds per timestamp-query tick (`limits.timestampPeriod`).
    /// `0.0` ⇒ no usable timestamp support; the compose GPU-render timer
    /// (`gpu_render_ns` telemetry) is then skipped.
    pub timestamp_period: f32,
}

impl VkContext {
    /// Whether this driver should advertise DRI3/Present syncobj.
    ///
    /// The implementation currently imports DRI3 syncobj fds as
    /// timeline semaphores with `OPAQUE_FD`. That works on the
    /// Vulkan stacks we have used for Venus/Mesa testing, but NVIDIA
    /// proprietary rejects the very first import with
    /// `ERROR_INITIALIZATION_FAILED` ("Failed to allocate semaphore
    /// device memory"). Advertising only DRI3 1.3 on that driver lets
    /// clients fall back to the older fence-fd path instead of dying
    /// on `ImportSyncobj`.
    #[must_use]
    pub fn supports_dri3_syncobj(&self) -> bool {
        !matches!(self.driver_id, vk::DriverId::NVIDIA_PROPRIETARY)
    }

    pub fn new() -> Result<Arc<Self>, VkInitError> {
        Self::new_with_drm_selection(None, true)
    }

    /// Build the rendering context on the Vulkan physical device belonging to
    /// the supplied KMS card. When the Vulkan implementation exposes DRM
    /// identities, a mismatched generic discrete/integrated preference is not
    /// allowed: scanout buffers must be allocated by the card that will scan
    /// them out. Implementations exposing no DRM identities retain the legacy
    /// scored fallback for portability.
    pub(crate) fn new_for_drm(
        primary: DrmDeviceKey,
        render: Option<DrmDeviceKey>,
    ) -> Result<Arc<Self>, VkInitError> {
        Self::new_with_drm_selection(Some(DrmDeviceSelection { primary, render }), true)
    }

    /// Build the minimal sink-side context used by copied scanout. It needs
    /// external memory/semaphores and synchronization2, but no compositor
    /// shaders, dynamic rendering, logic operations, or dual-source blending.
    pub(crate) fn new_transfer_for_drm(
        primary: DrmDeviceKey,
        render: Option<DrmDeviceKey>,
    ) -> Result<Arc<Self>, VkInitError> {
        Self::new_with_drm_selection(Some(DrmDeviceSelection { primary, render }), false)
    }

    fn new_with_drm_selection(
        requested_drm: Option<DrmDeviceSelection>,
        compositor_features: bool,
    ) -> Result<Arc<Self>, VkInitError> {
        let entry = unsafe { ash::Entry::load()? };
        let app_info = vk::ApplicationInfo::default()
            .application_name(c"yserver")
            .application_version(vk::make_api_version(0, 0, 1, 0))
            .engine_name(c"yserver-kms")
            .api_version(vk::API_VERSION_1_3);

        let ext_cstrs = super::instance::required_instance_extensions();
        let ext_ptrs: Vec<_> = ext_cstrs.iter().map(|c| c.as_ptr()).collect();

        // Validation layer in debug builds only, and only if the
        // installed Vulkan loader actually has it. Some environments
        // (e.g. the vng guest with no `vulkan-validation-layers`
        // installed) don't ship it; without this guard,
        // `vkCreateInstance` returns `VK_ERROR_LAYER_NOT_PRESENT` and
        // the whole backend falls back to pixman.
        //
        // Enable cases:
        //   - debug build: always try (validation-layer cost is fine).
        //   - release build with `YSERVER_VK_VALIDATION` set: opt-in
        //     for diagnosing release-mode-only bugs (e.g. perf-branch
        //     timeline-semaphore races) without rebuilding debug.
        let validation_layer_name = c"VK_LAYER_KHRONOS_validation";
        let validation_requested =
            cfg!(debug_assertions) || std::env::var_os("YSERVER_VK_VALIDATION").is_some();
        let validation_available =
            validation_requested && validation_layer_present(&entry, validation_layer_name);
        let layer_ptrs: Vec<*const c_char> = if validation_available {
            vec![validation_layer_name.as_ptr()]
        } else {
            Vec::new()
        };
        if validation_requested && !validation_available {
            log::warn!(
                "vulkan: validation layer requested but not present (install \
                 `vulkan-validation-layers` package); continuing without"
            );
        } else if validation_available {
            log::info!("vulkan: validation layer enabled");
        }

        let create_info = vk::InstanceCreateInfo::default()
            .application_info(&app_info)
            .enabled_extension_names(&ext_ptrs)
            .enabled_layer_names(&layer_ptrs);

        let instance = unsafe { entry.create_instance(&create_info, None)? };

        // Build the rest with manual error-cleanup. If any step after
        // create_instance fails, we must destroy the instance; same
        // applies to debug messenger / device once they exist.
        let debug_utils_instance = ash::ext::debug_utils::Instance::new(&entry, &instance);

        let debug_messenger = match create_debug_messenger(&debug_utils_instance) {
            Ok(m) => m,
            Err(e) => {
                unsafe { instance.destroy_instance(None) };
                return Err(e);
            }
        };

        let (physical_device, graphics_queue_family) =
            match pick_physical_device(&instance, requested_drm) {
                Ok(t) => t,
                Err(e) => {
                    unsafe {
                        if let Some(m) = debug_messenger {
                            debug_utils_instance.destroy_debug_utils_messenger(m, None);
                        }
                        instance.destroy_instance(None);
                    }
                    return Err(e);
                }
            };

        // Device extensions actually used by Phase 4.1.2's
        // Vulkan-first scanout path:
        //
        // - VK_KHR_external_memory_fd: vkGetMemoryFdKHR (export the
        //   bound image memory as a dma-buf).
        // - VK_EXT_external_memory_dma_buf: handle type `DMA_BUF` for
        //   the export (`ExternalMemoryImageCreateInfo` + the alloc).
        // - VK_KHR_external_semaphore_fd: vkGetSemaphoreFdKHR(SYNC_FD)
        //   for the IN_FENCE_FD handoff to KMS.
        //
        // Phase 4.2 reintroduction: VK_EXT_image_drm_format_modifier
        // is now requested for DRI3 tiled-image import. Drivers that
        // lack it (notably lavapipe at the time of writing) will still
        // function — `supported_modifiers` returns `[LINEAR]` per the
        // design §4 fallback matrix.
        //
        // Intentionally NOT requested:
        // - VK_KHR_swapchain — WSI is out of scope (design §1); KMS
        //   pageflip is our presentation path.
        // - VK_KHR_dynamic_rendering_local_read — only Phase 4.1.4.6
        //   ShaderRMW PictOps need this; deferred until that lands.
        //
        // The filter still drops anything the picked device doesn't
        // expose; on a healthy device every wanted extension makes
        // it through. The warning path remains as an early-fail signal
        // for misconfigured environments.
        let wanted: &[&CStr] = &[
            ash::khr::external_memory_fd::NAME,
            ash::ext::external_memory_dma_buf::NAME,
            ash::khr::external_semaphore_fd::NAME,
            ash::ext::image_drm_format_modifier::NAME,
        ];
        let supported_device_exts =
            match unsafe { instance.enumerate_device_extension_properties(physical_device) } {
                Ok(v) => v,
                Err(e) => {
                    unsafe {
                        if let Some(m) = debug_messenger {
                            debug_utils_instance.destroy_debug_utils_messenger(m, None);
                        }
                        instance.destroy_instance(None);
                    }
                    return Err(VkInitError::Vk(e));
                }
            };
        let device_extension_names: Vec<&'static CStr> = wanted
            .iter()
            .copied()
            .filter(|ext| {
                let ok = supported_device_exts.iter().any(|p| {
                    p.extension_name_as_c_str()
                        .map(|s| s == *ext)
                        .unwrap_or(false)
                });
                if !ok {
                    log::warn!(
                        "vulkan: physical device lacks {} — Vulkan-fed scanout will not work",
                        ext.to_string_lossy()
                    );
                }
                ok
            })
            .collect();
        let device_extensions: Vec<*const c_char> =
            device_extension_names.iter().map(|c| c.as_ptr()).collect();

        let priorities = [1.0_f32];
        let queue_info = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(graphics_queue_family)
            .queue_priorities(&priorities)];

        let mut features13 = vk::PhysicalDeviceVulkan13Features::default()
            .dynamic_rendering(compositor_features)
            .synchronization2(true);

        // `scalarBlockLayout` lets push-constant (and uniform/storage)
        // blocks use scalar packing rather than std140/std430's
        // 16-byte vec4 alignment, so a `vec4` after a stretch of
        // `vec2` fields lands directly after them — matching what
        // a `#[repr(C)]` Rust struct produces with no padding. This
        // sidesteps the alignment-mismatch bug class that produced
        // green text in `TextPushConsts` (vec4 expected at offset
        // 48 by std430, sat at offset 40 in Rust). The shaders that
        // rely on this declare `layout(scalar)` on the
        // `push_constant` block; the legacy `LogicFillPushConsts`
        // pad and the natural alignment of `RenderPushConsts` /
        // `CompositePushConsts` keep std430 layout intact and stay
        // compatible.
        // `timelineSemaphore` is core in Vulkan 1.2 and is required by
        // Phase 4.2.2's `import_drm_syncobj` (DRI3 ImportSyncobj path).
        // Harmless when the syncobj cap is false because the dispatcher
        // gate rejects requests before they reach the import call.
        let mut features12 = vk::PhysicalDeviceVulkan12Features::default()
            .scalar_block_layout(compositor_features)
            .timeline_semaphore(compositor_features);

        // `logicOp` enables the per-attachment logical-op state used
        // by the Phase 4.1.5 GC-function fill path (Xor / And / Or
        // / Invert / etc. — all 16 X11 GcFunction variants map 1:1
        // to `VkLogicOp`). `dualSrcBlend` enables the SRC1_* family
        // of blend factors used by the RENDER `component_alpha`
        // path (per-channel src alpha emitted from a second
        // fragment-shader output). Both are core Vulkan 1.0 and
        // universally supported on conformant drivers.
        let enabled_features = vk::PhysicalDeviceFeatures::default()
            .logic_op(compositor_features)
            .dual_src_blend(compositor_features);

        let device_info = vk::DeviceCreateInfo::default()
            .queue_create_infos(&queue_info)
            .enabled_extension_names(&device_extensions)
            .enabled_features(&enabled_features)
            .push_next(&mut features12)
            .push_next(&mut features13);

        let device = match unsafe { instance.create_device(physical_device, &device_info, None) } {
            Ok(d) => d,
            Err(e) => {
                unsafe {
                    if let Some(m) = debug_messenger {
                        debug_utils_instance.destroy_debug_utils_messenger(m, None);
                    }
                    instance.destroy_instance(None);
                }
                return Err(VkInitError::Vk(e));
            }
        };
        let graphics_queue = unsafe { device.get_device_queue(graphics_queue_family, 0) };
        let external_semaphore_fd =
            ash::khr::external_semaphore_fd::Device::new(&instance, &device);
        let external_memory_fd_supported =
            device_extension_names.contains(&ash::khr::external_memory_fd::NAME);
        let external_memory_fd = if external_memory_fd_supported {
            Some(ash::khr::external_memory_fd::Device::new(
                &instance, &device,
            ))
        } else {
            None
        };
        let image_drm_format_modifier =
            device_extension_names.contains(&ash::ext::image_drm_format_modifier::NAME);
        let image_drm_format_modifier_ext = if image_drm_format_modifier {
            Some(ash::ext::image_drm_format_modifier::Device::new(
                &instance, &device,
            ))
        } else {
            None
        };

        // Driver-id query. Diagnostic-only after the Vulkan-first
        // pivot — no path branches on it. Kept so future quirks can
        // re-introduce branches without re-querying.
        let mut driver_props = vk::PhysicalDeviceDriverProperties::default();
        let mut props2 = vk::PhysicalDeviceProperties2::default().push_next(&mut driver_props);
        unsafe {
            instance.get_physical_device_properties2(physical_device, &mut props2);
        }
        // Read from props2 (which mutably borrows driver_props) before
        // reading driver_props directly, so props2's borrow ends first.
        let device_type = props2.properties.device_type;
        // ns per timestamp-query tick (0.0 ⇒ device has no usable timestamp
        // support → gpu_render_ns telemetry stays 0). See `record_compose`.
        let timestamp_period = props2.properties.limits.timestamp_period;
        let driver_id = driver_props.driver_id;

        Ok(Arc::new(VkContext {
            entry,
            instance,
            debug_utils_instance,
            physical_device,
            device,
            external_semaphore_fd,
            external_memory_fd,
            image_drm_format_modifier_ext,
            image_drm_format_modifier,
            tfp_tiling_strategy: std::sync::OnceLock::new(),
            graphics_queue_family,
            graphics_queue,
            debug_messenger,
            driver_id,
            device_type,
            timestamp_period,
        }))
    }

    /// True when the picked Vulkan device is a software rasterizer
    /// (`VK_PHYSICAL_DEVICE_TYPE_CPU`, i.e. llvmpipe/lavapipe).
    ///
    /// Fine for headless rendering/tests, but driving **real KMS
    /// scanout** off a software device hard-hangs the machine on
    /// hardware that can't scan out the CPU/host-memory buffer
    /// (observed: nouveau on Pascal — the GPU's atomic commit wedges).
    /// The scanout bring-up refuses by default when this is true;
    /// see `PlatformBackend::from_platform_init`. Venus (virtio-gpu
    /// passthrough) reports `VIRTUAL_GPU`, not `CPU`, so it is not
    /// caught here.
    #[must_use]
    pub fn is_software_rasterizer(&self) -> bool {
        self.device_type == vk::PhysicalDeviceType::CPU
    }
}

/// Pre-flight check run BEFORE any DRM open / modeset: enumerate the
/// Vulkan physical devices (instance-level only — no VkDevice, no DRM,
/// no master, no screen blank) and refuse when every available device
/// is a software rasterizer (`CPU` type — llvmpipe/lavapipe).
///
/// Rationale: driving real KMS scanout off software Vulkan hard-hangs
/// the machine (observed on two KMS drivers: simpledrm and nvidia-drm —
/// no ping, no journal, power-cycle required). The in-bring-up guard in
/// `PlatformBackend::from_platform_init` exists too, but it runs after
/// the initial modeset; this preflight refuses while the console is
/// still intact, before yserver has touched the GPU at all.
///
/// Conservative on probe errors: if the loader / instance / enumeration
/// itself fails, this returns `Ok(())` and lets the real `VkContext::new`
/// produce its proper error — the preflight only blocks the one case it
/// can positively identify (all-software device list).
///
/// `YSERVER_ALLOW_SOFTWARE_VULKAN=1` skips the check (deliberate
/// software-scanout setups, e.g. lavapipe under vng). Venus reports
/// `VIRTUAL_GPU`, not `CPU`, and passes.
pub fn ensure_hardware_vulkan_for_scanout() -> Result<(), String> {
    if std::env::var_os("YSERVER_ALLOW_SOFTWARE_VULKAN").is_some() {
        log::warn!(
            "YSERVER_ALLOW_SOFTWARE_VULKAN set — skipping the hardware-Vulkan \
             preflight; a software rasterizer driving real KMS scanout can \
             hard-hang the machine"
        );
        return Ok(());
    }

    let entry = match unsafe { ash::Entry::load() } {
        Ok(e) => e,
        Err(e) => {
            log::warn!("hw-Vulkan preflight: loader unavailable ({e}); deferring to full init");
            return Ok(());
        }
    };
    let create_info = vk::InstanceCreateInfo::default();
    let instance = match unsafe { entry.create_instance(&create_info, None) } {
        Ok(i) => i,
        Err(e) => {
            log::warn!(
                "hw-Vulkan preflight: instance creation failed ({e}); deferring to full init"
            );
            return Ok(());
        }
    };

    // Collect (name, type) for every device, then destroy the instance
    // before deciding — no resource outlives the probe.
    let devices: Vec<(String, vk::PhysicalDeviceType)> =
        match unsafe { instance.enumerate_physical_devices() } {
            Ok(pds) => pds
                .into_iter()
                .map(|pd| {
                    let props = unsafe { instance.get_physical_device_properties(pd) };
                    let name = props
                        .device_name_as_c_str()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|_| "<unnamed>".into());
                    (name, props.device_type)
                })
                .collect(),
            Err(e) => {
                unsafe { instance.destroy_instance(None) };
                log::warn!("hw-Vulkan preflight: enumeration failed ({e}); deferring to full init");
                return Ok(());
            }
        };
    unsafe { instance.destroy_instance(None) };

    if devices.is_empty() {
        log::warn!("hw-Vulkan preflight: no Vulkan devices found; deferring to full init");
        return Ok(());
    }
    if devices
        .iter()
        .any(|(_, ty)| *ty != vk::PhysicalDeviceType::CPU)
    {
        return Ok(());
    }

    let listing = devices
        .iter()
        .map(|(name, ty)| format!("{name} ({ty:?})"))
        .collect::<Vec<_>>()
        .join(", ");
    let msg = format!(
        "every available Vulkan device is a software rasterizer: [{listing}]. \
         Driving real KMS scanout off software Vulkan (llvmpipe/lavapipe) \
         hard-hangs the machine. Refusing to start BEFORE touching the GPU. \
         Install a hardware Vulkan driver for the scanout GPU (radv / anv / nvk), \
         or check the GPU driver setup (e.g. proprietary driver removed but \
         nouveau not loaded leaves only llvmpipe). To override deliberately, \
         set YSERVER_ALLOW_SOFTWARE_VULKAN=1."
    );
    log::error!("hw-Vulkan preflight: {msg}");
    Err(msg)
}

fn validation_layer_present(entry: &ash::Entry, name: &CStr) -> bool {
    match unsafe { entry.enumerate_instance_layer_properties() } {
        Ok(layers) => layers
            .iter()
            .any(|l| l.layer_name_as_c_str().map(|s| s == name).unwrap_or(false)),
        Err(_) => false,
    }
}

fn create_debug_messenger(
    debug_utils_instance: &ash::ext::debug_utils::Instance,
) -> Result<Option<vk::DebugUtilsMessengerEXT>, VkInitError> {
    // Match the validation-layer enable rule from `VkContext::new`:
    // debug builds always install the messenger; release builds only
    // when `YSERVER_VK_VALIDATION` is set. Without the messenger the
    // validation layer has nowhere to report VUIDs and the layer is
    // effectively silent.
    if !cfg!(debug_assertions) && std::env::var_os("YSERVER_VK_VALIDATION").is_none() {
        return Ok(None);
    }
    let info = vk::DebugUtilsMessengerCreateInfoEXT::default()
        .message_severity(
            vk::DebugUtilsMessageSeverityFlagsEXT::WARNING
                | vk::DebugUtilsMessageSeverityFlagsEXT::ERROR,
        )
        .message_type(
            vk::DebugUtilsMessageTypeFlagsEXT::GENERAL
                | vk::DebugUtilsMessageTypeFlagsEXT::VALIDATION
                | vk::DebugUtilsMessageTypeFlagsEXT::PERFORMANCE,
        )
        .pfn_user_callback(Some(vk_debug_callback));
    Ok(Some(unsafe {
        debug_utils_instance.create_debug_utils_messenger(&info, None)?
    }))
}

impl Drop for VkContext {
    fn drop(&mut self) {
        unsafe {
            // Wait for all queue work; tearing down with in-flight CBs
            // is undefined behaviour.
            let _ = self.device.device_wait_idle();
            self.device.destroy_device(None);
            if let Some(m) = self.debug_messenger.take() {
                self.debug_utils_instance
                    .destroy_debug_utils_messenger(m, None);
            }
            self.instance.destroy_instance(None);
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum VkInitError {
    #[error("vulkan loader: {0}")]
    Loader(#[from] ash::LoadingError),
    #[error("vulkan: {0}")]
    Vk(vk::Result),
    #[error("no suitable physical device (need a graphics + transfer queue)")]
    NoSuitableDevice,
    #[error("no suitable Vulkan physical device matches DRM device {0}")]
    NoMatchingDrmDevice(String),
    #[error("multiple Vulkan physical devices match DRM device {0}")]
    AmbiguousDrmDevice(String),
}

impl From<vk::Result> for VkInitError {
    fn from(r: vk::Result) -> Self {
        VkInitError::Vk(r)
    }
}

#[derive(Debug, Clone, Copy)]
struct PhysicalDeviceCandidate {
    physical_device: vk::PhysicalDevice,
    graphics_queue_family: Option<u32>,
    score: u32,
    drm_identity: Option<VulkanDrmIdentity>,
}

fn pick_physical_device(
    instance: &ash::Instance,
    requested_drm: Option<DrmDeviceSelection>,
) -> Result<(vk::PhysicalDevice, u32), VkInitError> {
    let devices = unsafe { instance.enumerate_physical_devices() }?;

    let candidates: Vec<PhysicalDeviceCandidate> = devices
        .into_iter()
        .map(|physical_device| {
            let props = unsafe { instance.get_physical_device_properties(physical_device) };
            let score = match props.device_type {
                vk::PhysicalDeviceType::DISCRETE_GPU => 3,
                vk::PhysicalDeviceType::INTEGRATED_GPU => 2,
                vk::PhysicalDeviceType::VIRTUAL_GPU => 1,
                _ => 0,
            };
            Ok(PhysicalDeviceCandidate {
                physical_device,
                graphics_queue_family: pick_graphics_queue_family(instance, physical_device),
                score,
                drm_identity: physical_device_drm_identity(instance, physical_device)?,
            })
        })
        .collect::<Result<_, VkInitError>>()?;

    let selected = select_physical_device_candidate(&candidates, requested_drm)?;
    let queue_family = selected
        .graphics_queue_family
        .ok_or(VkInitError::NoSuitableDevice)?;
    Ok((selected.physical_device, queue_family))
}

fn select_physical_device_candidate(
    candidates: &[PhysicalDeviceCandidate],
    requested_drm: Option<DrmDeviceSelection>,
) -> Result<&PhysicalDeviceCandidate, VkInitError> {
    if let Some(requested) = requested_drm {
        let mut matching = candidates.iter().filter(|candidate| {
            candidate.graphics_queue_family.is_some()
                && candidate
                    .drm_identity
                    .is_some_and(|identity| drm_identity_matches(identity, requested))
        });
        if let Some(selected) = matching.next() {
            if matching.next().is_some() {
                return Err(VkInitError::AmbiguousDrmDevice(format_drm_selection(
                    requested,
                )));
            }
            log::info!(
                "vulkan: selected physical device matching DRM {}",
                format_drm_selection(requested)
            );
            return Ok(selected);
        }

        if candidates
            .iter()
            .any(|candidate| candidate.drm_identity.is_some())
        {
            return Err(VkInitError::NoMatchingDrmDevice(format_drm_selection(
                requested,
            )));
        }

        log::warn!(
            "vulkan: no physical device exposes VK_EXT_physical_device_drm; \
             falling back to generic device preference for DRM {}",
            format_drm_selection(requested)
        );
    }

    candidates
        .iter()
        .filter(|candidate| candidate.graphics_queue_family.is_some())
        .max_by_key(|candidate| candidate.score)
        .ok_or(VkInitError::NoSuitableDevice)
}

fn physical_device_drm_identity(
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
) -> Result<Option<VulkanDrmIdentity>, VkInitError> {
    let extensions = unsafe { instance.enumerate_device_extension_properties(physical_device) }?;
    let supports_drm_identity = extensions.iter().any(|property| {
        property
            .extension_name_as_c_str()
            .map(|name| name == ash::ext::physical_device_drm::NAME)
            .unwrap_or(false)
    });
    if !supports_drm_identity {
        return Ok(None);
    }

    let mut drm = vk::PhysicalDeviceDrmPropertiesEXT::default();
    let mut properties = vk::PhysicalDeviceProperties2::default().push_next(&mut drm);
    unsafe {
        instance.get_physical_device_properties2(physical_device, &mut properties);
    }
    Ok(Some(drm_identity_from_properties(&drm)))
}

fn drm_identity_from_properties(
    properties: &vk::PhysicalDeviceDrmPropertiesEXT<'_>,
) -> VulkanDrmIdentity {
    VulkanDrmIdentity {
        primary: drm_key(
            properties.has_primary,
            properties.primary_major,
            properties.primary_minor,
        ),
        render: drm_key(
            properties.has_render,
            properties.render_major,
            properties.render_minor,
        ),
    }
}

fn drm_key(has_node: vk::Bool32, major: i64, minor: i64) -> Option<DrmDeviceKey> {
    if has_node == vk::FALSE {
        return None;
    }
    Some(DrmDeviceKey {
        major: u32::try_from(major).ok()?,
        minor: u32::try_from(minor).ok()?,
    })
}

fn drm_identity_matches(identity: VulkanDrmIdentity, requested: DrmDeviceSelection) -> bool {
    let mut matched = false;
    if let Some(primary) = identity.primary {
        if primary != requested.primary {
            return false;
        }
        matched = true;
    }
    if let (Some(render), Some(requested_render)) = (identity.render, requested.render) {
        if render != requested_render {
            return false;
        }
        matched = true;
    }
    matched
}

fn format_drm_selection(selection: DrmDeviceSelection) -> String {
    selection.render.map_or_else(
        || format!("primary {}", selection.primary),
        |render| format!("primary {}, render {render}", selection.primary),
    )
}

fn pick_graphics_queue_family(instance: &ash::Instance, pd: vk::PhysicalDevice) -> Option<u32> {
    let qfp = unsafe { instance.get_physical_device_queue_family_properties(pd) };
    qfp.iter().enumerate().find_map(|(i, p)| {
        if p.queue_flags
            .contains(vk::QueueFlags::GRAPHICS | vk::QueueFlags::TRANSFER)
        {
            Some(u32::try_from(i).expect("queue family index fits in u32"))
        } else {
            None
        }
    })
}

unsafe extern "system" fn vk_debug_callback(
    severity: vk::DebugUtilsMessageSeverityFlagsEXT,
    _ty: vk::DebugUtilsMessageTypeFlagsEXT,
    callback_data: *const vk::DebugUtilsMessengerCallbackDataEXT<'_>,
    _user_data: *mut c_void,
) -> vk::Bool32 {
    // Validation can call this with a null callback_data on some
    // drivers; defend against that.
    if callback_data.is_null() {
        return vk::FALSE;
    }
    let data = unsafe { &*callback_data };
    let msg = if data.p_message.is_null() {
        "<no message>"
    } else {
        unsafe { CStr::from_ptr(data.p_message) }
            .to_str()
            .unwrap_or("<non-utf8 message>")
    };
    if severity.contains(vk::DebugUtilsMessageSeverityFlagsEXT::ERROR) {
        log::error!("vk: {msg}");
    } else if severity.contains(vk::DebugUtilsMessageSeverityFlagsEXT::WARNING) {
        log::warn!("vk: {msg}");
    }
    // INFO/VERBOSE intentionally suppressed — too noisy.
    vk::FALSE
}

#[cfg(test)]
mod tests {
    use ash::vk::Handle as _;

    use super::*;

    fn key(minor: u32) -> DrmDeviceKey {
        DrmDeviceKey { major: 226, minor }
    }

    fn identity(primary: Option<u32>, render: Option<u32>) -> VulkanDrmIdentity {
        VulkanDrmIdentity {
            primary: primary.map(key),
            render: render.map(key),
        }
    }

    fn candidate(
        handle: u64,
        score: u32,
        drm_identity: Option<VulkanDrmIdentity>,
    ) -> PhysicalDeviceCandidate {
        PhysicalDeviceCandidate {
            physical_device: vk::PhysicalDevice::from_raw(handle),
            graphics_queue_family: Some(0),
            score,
            drm_identity,
        }
    }

    #[test]
    fn drm_properties_preserve_primary_and_render_keys() {
        let properties = vk::PhysicalDeviceDrmPropertiesEXT {
            has_primary: vk::TRUE,
            has_render: vk::TRUE,
            primary_major: 226,
            primary_minor: 1,
            render_major: 226,
            render_minor: 129,
            ..Default::default()
        };

        assert_eq!(
            drm_identity_from_properties(&properties),
            identity(Some(1), Some(129))
        );
    }

    #[test]
    fn drm_properties_reject_negative_node_numbers() {
        let properties = vk::PhysicalDeviceDrmPropertiesEXT {
            has_primary: vk::TRUE,
            has_render: vk::FALSE,
            primary_major: -1,
            primary_minor: 0,
            ..Default::default()
        };

        assert_eq!(
            drm_identity_from_properties(&properties),
            identity(None, None)
        );
    }

    #[test]
    fn drm_selection_overrides_generic_device_score() {
        let candidates = [
            candidate(1, 3, Some(identity(Some(0), Some(128)))),
            candidate(2, 2, Some(identity(Some(1), Some(129)))),
        ];

        let selected = select_physical_device_candidate(
            &candidates,
            Some(DrmDeviceSelection {
                primary: key(1),
                render: Some(key(129)),
            }),
        )
        .unwrap();

        assert_eq!(selected.physical_device.as_raw(), 2);
    }

    #[test]
    fn drm_selection_accepts_render_only_vulkan_identity() {
        let candidates = [candidate(7, 1, Some(identity(None, Some(129))))];

        let selected = select_physical_device_candidate(
            &candidates,
            Some(DrmDeviceSelection {
                primary: key(1),
                render: Some(key(129)),
            }),
        )
        .unwrap();

        assert_eq!(selected.physical_device.as_raw(), 7);
    }

    #[test]
    fn drm_selection_rejects_conflicting_render_node() {
        let candidates = [candidate(1, 3, Some(identity(Some(1), Some(130))))];

        let error = select_physical_device_candidate(
            &candidates,
            Some(DrmDeviceSelection {
                primary: key(1),
                render: Some(key(129)),
            }),
        )
        .unwrap_err();

        assert!(matches!(error, VkInitError::NoMatchingDrmDevice(_)));
    }

    #[test]
    fn drm_selection_rejects_duplicate_vulkan_claims() {
        let candidates = [
            candidate(1, 3, Some(identity(Some(1), Some(129)))),
            candidate(2, 2, Some(identity(Some(1), Some(129)))),
        ];

        let error = select_physical_device_candidate(
            &candidates,
            Some(DrmDeviceSelection {
                primary: key(1),
                render: Some(key(129)),
            }),
        )
        .unwrap_err();

        assert!(matches!(error, VkInitError::AmbiguousDrmDevice(_)));
    }

    #[test]
    fn drm_selection_does_not_fallback_when_any_identity_is_available() {
        let candidates = [
            candidate(1, 3, Some(identity(Some(0), Some(128)))),
            candidate(2, 2, None),
        ];

        let error = select_physical_device_candidate(
            &candidates,
            Some(DrmDeviceSelection {
                primary: key(1),
                render: Some(key(129)),
            }),
        )
        .unwrap_err();

        assert!(matches!(error, VkInitError::NoMatchingDrmDevice(_)));
    }

    #[test]
    fn drm_selection_falls_back_only_when_no_identity_is_available() {
        let candidates = [candidate(1, 1, None), candidate(2, 3, None)];

        let selected = select_physical_device_candidate(
            &candidates,
            Some(DrmDeviceSelection {
                primary: key(1),
                render: Some(key(129)),
            }),
        )
        .unwrap();

        assert_eq!(selected.physical_device.as_raw(), 2);
    }
}
