//! KMS-facing render-node helpers.
//!
//! The platform-specific discovery lives in `platform::drm`; this module keeps
//! the rest of the KMS backend from depending on that discovery shape directly.

use std::{
    io,
    os::fd::{AsFd, OwnedFd},
    path::{Path, PathBuf},
};

#[cfg(target_os = "linux")]
use crate::platform::{
    drm::{DrmNodeKind, DrmPlatform},
    drm_linux::LinuxDrmPlatform,
};

pub(crate) fn open_for_card<F: AsFd>(card_fd: F) -> io::Result<(OwnedFd, PathBuf)> {
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
        Ok((fd, render.path))
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
