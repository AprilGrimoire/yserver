//! KMS-facing render-node helpers.
//!
//! The platform-specific discovery lives in `platform::drm`; this module keeps
//! the rest of the KMS backend from depending on that discovery shape directly.

use std::{
    io,
    os::fd::{AsFd, AsRawFd, OwnedFd, RawFd},
    path::Path,
};

use crate::platform::drm::{DrmDeviceKey, DrmNode};

#[cfg(target_os = "linux")]
use crate::platform::{
    drm::{DrmNodeKind, DrmPlatform},
    drm_linux::LinuxDrmPlatform,
};

/// An opened render node and the stable platform identity of that exact node.
///
/// Keeping these values in one type makes it impossible for callers to retain
/// a path/key without the corresponding live fd, or vice versa.
pub(crate) struct OpenedRenderNode {
    fd: OwnedFd,
    node: DrmNode,
}

impl OpenedRenderNode {
    #[must_use]
    pub(crate) fn raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    #[must_use]
    pub(crate) fn path(&self) -> &Path {
        &self.node.path
    }

    #[must_use]
    pub(crate) fn key(&self) -> DrmDeviceKey {
        self.node.key
    }

    #[cfg(test)]
    pub(crate) fn for_tests(fd: OwnedFd) -> Self {
        Self {
            fd,
            node: DrmNode {
                path: "/dev/null".into(),
                key: DrmDeviceKey { major: 0, minor: 0 },
                kind: crate::platform::drm::DrmNodeKind::Render,
            },
        }
    }
}

pub(crate) fn open_for_card<F: AsFd>(card_fd: F) -> io::Result<OpenedRenderNode> {
    #[cfg(target_os = "linux")]
    {
        let platform = LinuxDrmPlatform;
        let primary = platform.node_from_fd(card_fd.as_fd(), DrmNodeKind::Primary)?;
        let render = platform.render_node_for_primary(&primary)?.ok_or_else(|| {
            io::Error::other(format!(
                "no DRM render node found for card with rdev {}",
                primary.key
            ))
        })?;
        let fd = platform.open_node(&render)?;
        Ok(OpenedRenderNode { fd, node: render })
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = card_fd;
        Err(io::Error::other(
            "DRM render-node discovery is not implemented on this platform",
        ))
    }
}

pub(crate) fn open_fresh(path: &Path) -> io::Result<OwnedFd> {
    crate::platform::drm::open_path_cloexec(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_fresh_fails_for_missing_path() {
        let path = std::env::temp_dir().join("yserver-render-node-test-nonexistent");
        let _ = std::fs::remove_file(&path);
        assert!(open_fresh(&path).is_err());
    }
}
