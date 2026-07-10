//! Platform-neutral DRM device discovery boundary.
//!
//! The rest of yserver should reason in terms of DRM nodes and stable
//! device keys. Concrete operating-system details, such as how primary
//! and render nodes are enumerated or related, live in platform-specific
//! modules such as `drm_linux`.

use std::{
    ffi::CString,
    fmt, io,
    os::{
        fd::{BorrowedFd, FromRawFd, OwnedFd},
        unix::ffi::OsStrExt,
    },
    path::{Path, PathBuf},
};

use ::drm::control::{Device as _, Mode as DrmMode, connector, crtc, plane, property};

/// Stable kernel device identity for a DRM node.
///
/// This is the `st_rdev` major/minor of a DRM device node, not a
/// volatile card number. Vulkan's `VK_EXT_physical_device_drm` exposes
/// the same shape, making this the right future join key for PRIME
/// provider matching.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct DrmDeviceKey {
    pub(crate) major: u32,
    pub(crate) minor: u32,
}

impl fmt::Display for DrmDeviceKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.major, self.minor)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DrmNodeKind {
    Primary,
    Render,
}

/// One DRM device node discovered by the platform layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DrmNode {
    pub(crate) path: PathBuf,
    pub(crate) key: DrmDeviceKey,
    pub(crate) kind: DrmNodeKind,
}

/// A KMS-capable primary node, tagged with whether it currently has a
/// connected display. Built by probing the DRM control API after
/// platform enumeration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct KmsCardCandidate {
    pub(crate) node: DrmNode,
    pub(crate) has_connected_connector: bool,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Mode {
    pub(crate) name: String,
    pub(crate) width: u16,
    pub(crate) height: u16,
    pub(crate) vrefresh: u32,
    pub(crate) preferred: bool,
    /// Real kernel timing (from `drmModeModeInfo`), carried through so
    /// the RANDR `ModeInfo` reply reproduces the exact fractional refresh
    /// Xorg reports (e.g. 59.95, not integer 60). `clock_khz == 0` means
    /// timing is unknown (synthetic/test modes) and the RANDR layer falls
    /// back to synthesising blanking. See `yserver_core::randr::ModeTiming`.
    pub(crate) clock_khz: u32,
    pub(crate) hsync_start: u16,
    pub(crate) hsync_end: u16,
    pub(crate) htotal: u16,
    pub(crate) vsync_start: u16,
    pub(crate) vsync_end: u16,
    pub(crate) vtotal: u16,
    /// Raw `DRM_MODE_FLAG_*` bits; mapped to RANDR flags at report time.
    pub(crate) flags: u32,
}

/// Normalized KMS topology for one connected output.
#[derive(Debug)]
pub(crate) struct Output {
    pub(crate) connector: connector::Handle,
    pub(crate) connector_name: String,
    pub(crate) crtc: crtc::Handle,
    pub(crate) plane: plane::Handle,
    pub(crate) mode: DrmMode,
    pub(crate) picked: Mode,
    pub(crate) plane_fb_id_prop: property::Handle,
    pub(crate) plane_crtc_id_prop: property::Handle,
    /// Cached explicit-sync plane property. `None` means the driver
    /// did not expose it during modeset discovery; page-flip submission
    /// falls back to lookup so compatibility stays unchanged.
    pub(crate) plane_in_fence_fd_prop: Option<property::Handle>,
    /// Cached explicit-sync CRTC property. See
    /// [`Self::plane_in_fence_fd_prop`].
    pub(crate) crtc_out_fence_ptr_prop: Option<property::Handle>,
    /// DRM modifiers accepted by the primary plane for XRGB8888
    /// scanout, parsed from the optional IN_FORMATS property. Empty
    /// means the driver did not expose IN_FORMATS or parsing failed;
    /// callers should fall back to conservative legacy probing.
    pub(crate) scanout_modifiers: Vec<u64>,
    /// EDID-derived physical width of the connected display in
    /// millimeters. 0 if the connector did not report a size (e.g.
    /// virtio-gpu, displays without EDID); callers should fall back
    /// to a 96-DPI synthesis from pixel dimensions.
    pub(crate) mm_width: u32,
    /// EDID-derived physical height; see [`Self::mm_width`].
    pub(crate) mm_height: u32,
    /// Raw EDID blob read from the connector's `EDID` property (128
    /// bytes, or 256 with an extension block). Empty when the connector
    /// exposes no EDID (virtio-gpu, headless). Served to RANDR clients
    /// as the `EDID`/`EDID_DATA` output property so monitor-identity
    /// matching (mate/mutter `monitors.xml`) works.
    pub(crate) edid: Vec<u8>,
    /// RANDR `ConnectorType` property value name mapped from the DRM
    /// connector interface (`"DisplayPort"`, `"HDMI"`, `"DVI-D"`,
    /// `"VGA"`, `"Panel"`, ...; `"unknown"` when unmappable).
    pub(crate) connector_type: String,
    /// The connector's full local mode list, preferred-first, as
    /// reported by the kernel/EDID. `picked` is the boot default and
    /// is always present in this list. Used by RANDR to advertise the
    /// selectable mode set (`GetOutputInfo` / `GetScreenResources`) and
    /// by `apply_crtc_config` to resolve a client-requested mode.
    pub(crate) modes: Vec<Mode>,
}

/// Platform boundary for DRM node enumeration and node relationship
/// queries. Common KMS/RANDR code should use this instead of directly
/// walking OS-specific device filesystems.
pub(crate) trait DrmPlatform {
    fn enumerate_primary_nodes(&self) -> io::Result<Vec<DrmNode>>;
    fn enumerate_render_nodes(&self) -> io::Result<Vec<DrmNode>>;
    fn node_from_fd(&self, fd: BorrowedFd<'_>, kind: DrmNodeKind) -> io::Result<DrmNode>;
    fn render_node_for_primary(&self, primary: &DrmNode) -> io::Result<Option<DrmNode>>;
    fn open_node(&self, node: &DrmNode) -> io::Result<OwnedFd>;
}

/// Platform boundary for KMS connector/output discovery on an already-open
/// primary DRM node.
///
/// The returned `Output` records are yserver's normalized KMS topology:
/// connected connector, selected mode, assigned CRTC, assigned primary
/// plane, scanout modifiers, EDID, and RANDR-facing connector metadata.
pub(crate) trait KmsPlatform {
    fn discover_outputs(&self, device: &crate::drm::Device) -> io::Result<Vec<Output>>;
}

#[cfg(target_os = "linux")]
pub(crate) fn primary_node_from_fd(fd: BorrowedFd<'_>) -> io::Result<DrmNode> {
    crate::platform::drm_linux::LinuxDrmPlatform.node_from_fd(fd, DrmNodeKind::Primary)
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn primary_node_from_fd(_fd: BorrowedFd<'_>) -> io::Result<DrmNode> {
    Err(io::Error::other(
        "DRM device identity is not implemented on this platform",
    ))
}

/// Discover the currently connected KMS outputs on `device`.
#[cfg(target_os = "linux")]
pub(crate) fn discover_outputs(device: &crate::drm::Device) -> io::Result<Vec<Output>> {
    crate::platform::drm_linux::LinuxDrmPlatform.discover_outputs(device)
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn discover_outputs(_device: &crate::drm::Device) -> io::Result<Vec<Output>> {
    Err(io::Error::other(
        "KMS output discovery is not implemented on this platform",
    ))
}

/// Resolve the KMS cards yserver should open at startup.
///
/// `YSERVER_DRM_DEVICE` remains an explicit override. Otherwise this
/// delegates enumeration and node-relationship details to the current
/// platform implementation, then applies yserver's shared policy:
/// keep only KMS-capable primary nodes and order the returned list with
/// the primary scanout candidate first. An empty list is a valid headless
/// result; the KMS backend can still serve X11 clients without scanout.
pub(crate) fn resolve_default_kms_devices() -> io::Result<Vec<PathBuf>> {
    if let Ok(explicit) = std::env::var("YSERVER_DRM_DEVICE") {
        return Ok(vec![PathBuf::from(explicit)]);
    }
    resolve_default_kms_devices_for_system()
}

#[cfg(target_os = "linux")]
fn resolve_default_kms_devices_for_system() -> io::Result<Vec<PathBuf>> {
    resolve_default_kms_devices_with(&crate::platform::drm_linux::LinuxDrmPlatform)
}

#[cfg(not(target_os = "linux"))]
fn resolve_default_kms_devices_for_system() -> io::Result<Vec<PathBuf>> {
    Err(io::Error::other(
        "automatic DRM device discovery is not implemented on this platform; \
         set YSERVER_DRM_DEVICE to a primary DRM node",
    ))
}

fn resolve_default_kms_devices_with(platform: &impl DrmPlatform) -> io::Result<Vec<PathBuf>> {
    let candidates = discover_kms_candidates(platform)?;
    let ordered = order_kms_card_candidates(candidates);
    if let Some(chosen) = ordered.first() {
        log::info!(
            "yserver: selected primary DRM device {} (connected_display={})",
            chosen.node.path.display(),
            chosen.has_connected_connector
        );
        for secondary in ordered.iter().skip(1) {
            log::info!(
                "yserver: discovered secondary DRM device {} (connected_display={})",
                secondary.node.path.display(),
                secondary.has_connected_connector
            );
        }
        return Ok(ordered
            .into_iter()
            .map(|candidate| candidate.node.path)
            .collect());
    }

    log::info!("yserver: no KMS-capable DRM devices discovered; starting headless");
    Ok(Vec::new())
}

/// Return the index of the primary DRM device from KMS-capable candidates.
///
/// `candidates` must be in platform priority order. The first candidate
/// with a connected connector wins; if none report a connection we fall
/// back to the first candidate. The fallback preserves headless paths.
fn primary_kms_card_candidate_index(candidates: &[KmsCardCandidate]) -> Option<usize> {
    candidates
        .iter()
        .position(|c| c.has_connected_connector)
        .or(if candidates.is_empty() { None } else { Some(0) })
}

/// Return KMS-capable candidates with the selected primary first.
///
/// The remaining candidates keep their original platform enumeration
/// order. This lets startup carry a full device vector while preserving
/// the existing "one primary scanout device" policy.
fn order_kms_card_candidates(mut candidates: Vec<KmsCardCandidate>) -> Vec<KmsCardCandidate> {
    let Some(primary_idx) = primary_kms_card_candidate_index(&candidates) else {
        return candidates;
    };
    let primary = candidates.remove(primary_idx);
    let mut ordered = Vec::with_capacity(candidates.len() + 1);
    ordered.push(primary);
    ordered.extend(candidates);
    ordered
}

fn discover_kms_candidates(platform: &impl DrmPlatform) -> io::Result<Vec<KmsCardCandidate>> {
    let nodes = platform.enumerate_primary_nodes()?;
    let mut candidates = Vec::new();
    let mut reasons: Vec<String> = Vec::new();
    for node in nodes {
        let path = node.path.to_string_lossy().into_owned();
        let device = match crate::drm::Device::open(&path) {
            Ok(d) => d,
            Err(err) => {
                log::info!("yserver: skipping {path}: open failed: {err}");
                reasons.push(format!("{path}: open failed: {err}"));
                continue;
            }
        };

        // Render-only drivers (asahi GPU, etc.) return EOPNOTSUPP
        // here. Anything else is not a KMS scanout card for yserver.
        let resources = match device.resource_handles() {
            Ok(r) => r,
            Err(err) => {
                log::info!("yserver: skipping {path}: not KMS-capable: {err}");
                reasons.push(format!("{path}: not KMS-capable: {err}"));
                continue;
            }
        };

        // Use cached connector state (force_probe=false), matching
        // discover_outputs; a full probe per card on startup is needless.
        let has_connected_connector = resources.connectors().iter().any(|&handle| {
            device
                .get_connector(handle, false)
                .is_ok_and(|info| info.state() == connector::State::Connected)
        });
        log::info!(
            "yserver: candidate {path}: KMS-capable, connected_display={has_connected_connector}"
        );
        candidates.push(KmsCardCandidate {
            node,
            has_connected_connector,
        });
    }

    if candidates.is_empty() && !reasons.is_empty() {
        log::warn!(
            "yserver: no KMS-capable DRM devices could be opened; starting headless. Tried:\n  {}",
            reasons.join("\n  ")
        );
    }
    Ok(candidates)
}

#[cfg(test)]
fn pick_kms_card_candidate(candidates: &[KmsCardCandidate]) -> Option<&KmsCardCandidate> {
    primary_kms_card_candidate_index(candidates).map(|idx| &candidates[idx])
}

pub(crate) fn open_path_cloexec(path: &Path) -> io::Result<OwnedFd> {
    let cstr = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::other(format!("path contains nul byte: {}", path.display())))?;
    let raw = unsafe { libc::open(cstr.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(path: &str, connected: bool) -> KmsCardCandidate {
        KmsCardCandidate {
            node: DrmNode {
                path: PathBuf::from(path),
                key: DrmDeviceKey {
                    major: 226,
                    minor: 0,
                },
                kind: DrmNodeKind::Primary,
            },
            has_connected_connector: connected,
        }
    }

    #[test]
    fn pick_kms_card_candidate_returns_none_for_empty() {
        assert!(pick_kms_card_candidate(&[]).is_none());
    }

    #[test]
    fn pick_kms_card_candidate_prefers_connected_over_earlier_disconnected() {
        // The iGPU+dGPU case from issue #62: the earlier platform node has no
        // display, but the later one drives the monitor.
        let cands = [
            candidate("/mock/primary1", false),
            candidate("/mock/primary2", true),
        ];
        assert_eq!(
            pick_kms_card_candidate(&cands).unwrap().node.path,
            PathBuf::from("/mock/primary2")
        );
    }

    #[test]
    fn pick_kms_card_candidate_keeps_order_among_connected() {
        let cands = [
            candidate("/mock/primary0", true),
            candidate("/mock/primary1", true),
        ];
        assert_eq!(
            pick_kms_card_candidate(&cands).unwrap().node.path,
            PathBuf::from("/mock/primary0")
        );
    }

    #[test]
    fn pick_kms_card_candidate_falls_back_to_first_when_none_connected() {
        // Headless / vng: KMS-capable but no connectors report Connected.
        let cands = [
            candidate("/mock/primary0", false),
            candidate("/mock/primary1", false),
        ];
        assert_eq!(
            pick_kms_card_candidate(&cands).unwrap().node.path,
            PathBuf::from("/mock/primary0")
        );
    }

    #[test]
    fn pick_kms_card_candidate_single_disconnected_card_is_still_selected() {
        let cands = [candidate("/mock/primary0", false)];
        assert_eq!(
            pick_kms_card_candidate(&cands).unwrap().node.path,
            PathBuf::from("/mock/primary0")
        );
    }

    #[test]
    fn order_kms_card_candidates_moves_primary_connected_candidate_first() {
        let ordered = order_kms_card_candidates(vec![
            candidate("/mock/primary0", false),
            candidate("/mock/primary1", true),
            candidate("/mock/primary2", false),
        ]);
        let paths: Vec<PathBuf> = ordered.into_iter().map(|c| c.node.path).collect();
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/mock/primary1"),
                PathBuf::from("/mock/primary0"),
                PathBuf::from("/mock/primary2"),
            ]
        );
    }

    #[test]
    fn order_kms_card_candidates_keeps_platform_order_when_first_is_primary() {
        let ordered = order_kms_card_candidates(vec![
            candidate("/mock/primary0", true),
            candidate("/mock/primary1", true),
            candidate("/mock/primary2", false),
        ]);
        let paths: Vec<PathBuf> = ordered.into_iter().map(|c| c.node.path).collect();
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/mock/primary0"),
                PathBuf::from("/mock/primary1"),
                PathBuf::from("/mock/primary2"),
            ]
        );
    }

    #[test]
    fn open_path_cloexec_fails_for_missing_path() {
        let path = std::env::temp_dir().join("yserver-drm-platform-test-nonexistent");
        let _ = std::fs::remove_file(&path);
        assert!(open_path_cloexec(&path).is_err());
    }
}
