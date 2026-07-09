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

use ::drm::control::{Device as _, connector};

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

/// Resolve the KMS card yserver should drive at startup.
///
/// `YSERVER_DRM_DEVICE` remains an explicit override. Otherwise this
/// delegates enumeration and node-relationship details to the current
/// platform implementation, then applies yserver's shared policy:
/// keep only KMS-capable primary nodes and prefer one with a connected
/// display.
pub(crate) fn resolve_default_kms_device() -> io::Result<PathBuf> {
    if let Ok(explicit) = std::env::var("YSERVER_DRM_DEVICE") {
        return Ok(PathBuf::from(explicit));
    }
    resolve_default_kms_device_for_system()
}

#[cfg(target_os = "linux")]
fn resolve_default_kms_device_for_system() -> io::Result<PathBuf> {
    resolve_default_kms_device_with(&crate::platform::drm_linux::LinuxDrmPlatform)
}

#[cfg(not(target_os = "linux"))]
fn resolve_default_kms_device_for_system() -> io::Result<PathBuf> {
    Err(io::Error::other(
        "automatic DRM device discovery is not implemented on this platform; \
         set YSERVER_DRM_DEVICE to a primary DRM node",
    ))
}

fn resolve_default_kms_device_with(platform: &impl DrmPlatform) -> io::Result<PathBuf> {
    let candidates = discover_kms_candidates(platform)?;
    if let Some(chosen) = pick_kms_card_candidate(&candidates) {
        log::info!(
            "yserver: selected DRM device {} (connected_display={})",
            chosen.node.path.display(),
            chosen.has_connected_connector
        );
        return Ok(chosen.node.path.clone());
    }

    Err(io::Error::other(format!(
        "no KMS-capable DRM device found. Tried:\n  {}\n\
         Override with YSERVER_DRM_DEVICE=<primary DRM node>.",
        if candidates.is_empty() {
            "(no platform primary DRM nodes)".to_string()
        } else {
            candidates
                .iter()
                .map(|c| c.node.path.display().to_string())
                .collect::<Vec<_>>()
                .join("\n  ")
        }
    )))
}

/// Choose a DRM device from KMS-capable candidates, preferring one that
/// has at least one connected connector.
///
/// `candidates` must be in platform priority order. The first candidate
/// with a connected connector wins; if none report a connection we fall
/// back to the first candidate. The fallback preserves headless paths.
fn pick_kms_card_candidate(candidates: &[KmsCardCandidate]) -> Option<&KmsCardCandidate> {
    candidates
        .iter()
        .find(|c| c.has_connected_connector)
        .or_else(|| candidates.first())
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
        return Err(io::Error::other(format!(
            "no KMS-capable DRM device found. Tried:\n  {}",
            reasons.join("\n  ")
        )));
    }
    Ok(candidates)
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
    fn open_path_cloexec_fails_for_missing_path() {
        let path = std::env::temp_dir().join("yserver-drm-platform-test-nonexistent");
        let _ = std::fs::remove_file(&path);
        assert!(open_path_cloexec(&path).is_err());
    }
}
