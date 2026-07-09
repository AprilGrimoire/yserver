//! Linux DRM node discovery.
//!
//! This module owns yserver's Linux assumptions: `/dev/dri` node naming,
//! sysfs parent matching through `/sys/dev/char`, and `st_rdev` major/minor
//! extraction. Shared KMS policy stays in `platform::drm`.

use std::{
    fs,
    io::{self, ErrorKind},
    os::{
        fd::{AsRawFd, BorrowedFd, OwnedFd},
        unix::fs::MetadataExt,
    },
    path::PathBuf,
};

use crate::platform::drm::{DrmDeviceKey, DrmNode, DrmNodeKind, DrmPlatform, open_path_cloexec};

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct LinuxDrmPlatform;

impl DrmPlatform for LinuxDrmPlatform {
    fn enumerate_primary_nodes(&self) -> io::Result<Vec<DrmNode>> {
        enumerate_dri_nodes("card", DrmNodeKind::Primary)
    }

    fn enumerate_render_nodes(&self) -> io::Result<Vec<DrmNode>> {
        enumerate_dri_nodes("renderD", DrmNodeKind::Render)
    }

    fn node_from_fd(&self, fd: BorrowedFd<'_>, kind: DrmNodeKind) -> io::Result<DrmNode> {
        Ok(DrmNode {
            path: PathBuf::new(),
            key: key_from_fd(fd)?,
            kind,
        })
    }

    fn render_node_for_primary(&self, primary: &DrmNode) -> io::Result<Option<DrmNode>> {
        debug_assert_eq!(primary.kind, DrmNodeKind::Primary);
        if let Some(path) = render_node_path_via_device_dir(primary.key)? {
            return node_for_path(path, DrmNodeKind::Render).map(Some);
        }
        render_node_via_dev_walk(self, primary.key)
    }

    fn open_node(&self, node: &DrmNode) -> io::Result<OwnedFd> {
        debug_assert!(matches!(
            node.kind,
            DrmNodeKind::Primary | DrmNodeKind::Render
        ));
        open_path_cloexec(&node.path)
    }
}

fn enumerate_dri_nodes(prefix: &str, kind: DrmNodeKind) -> io::Result<Vec<DrmNode>> {
    let entries = match fs::read_dir("/dev/dri") {
        Ok(entries) => entries,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            return Err(io::Error::new(
                err.kind(),
                format!("read_dir(/dev/dri): {err}"),
            ));
        }
    };
    let mut nodes = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with(prefix) {
            continue;
        }
        nodes.push(node_for_path(entry.path(), kind)?);
    }
    nodes.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(nodes)
}

fn node_for_path(path: PathBuf, kind: DrmNodeKind) -> io::Result<DrmNode> {
    let meta = fs::metadata(&path)?;
    Ok(DrmNode {
        path,
        key: key_from_rdev(meta.rdev()),
        kind,
    })
}

/// Fast path: `/sys/dev/char/<major>:<minor>/device/drm/renderD*`.
fn render_node_path_via_device_dir(primary: DrmDeviceKey) -> io::Result<Option<PathBuf>> {
    let dir = PathBuf::from(format!("/sys/dev/char/{primary}/device/drm"));
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with("renderD") {
            return Ok(Some(PathBuf::from("/dev/dri").join(&*name_str)));
        }
    }
    Ok(None)
}

fn render_node_via_dev_walk(
    platform: &impl DrmPlatform,
    primary: DrmDeviceKey,
) -> io::Result<Option<DrmNode>> {
    let render_nodes = platform.enumerate_render_nodes()?;
    let primary_parent = device_parent_for(primary).ok();
    if let Some(primary_parent) = primary_parent.as_deref() {
        for node in &render_nodes {
            if let Ok(render_parent) = device_parent_for(node.key)
                && render_parent == primary_parent
            {
                return Ok(Some(node.clone()));
            }
        }
    }
    Ok(render_nodes.into_iter().next())
}

fn device_parent_for(key: DrmDeviceKey) -> io::Result<PathBuf> {
    fs::canonicalize(format!("/sys/dev/char/{key}/device"))
}

fn key_from_fd(fd: BorrowedFd<'_>) -> io::Result<DrmDeviceKey> {
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::fstat(fd.as_raw_fd(), &mut stat) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    #[allow(clippy::useless_conversion)]
    Ok(key_from_rdev(u64::from(stat.st_rdev)))
}

fn key_from_rdev(rdev: u64) -> DrmDeviceKey {
    DrmDeviceKey {
        major: libc_major(rdev),
        minor: libc_minor(rdev),
    }
}

#[allow(clippy::cast_possible_truncation)]
fn libc_major(rdev: u64) -> u32 {
    libc::major(rdev) as u32
}

#[allow(clippy::cast_possible_truncation)]
fn libc_minor(rdev: u64) -> u32 {
    libc::minor(rdev) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_from_rdev_round_trip() {
        let dev = libc::makedev(226, 128);
        assert_eq!(
            key_from_rdev(dev),
            DrmDeviceKey {
                major: 226,
                minor: 128,
            }
        );
    }
}
