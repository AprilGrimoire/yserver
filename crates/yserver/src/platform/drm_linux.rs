//! Linux DRM node discovery.
//!
//! This module owns yserver's Linux assumptions: `/dev/dri` node naming,
//! sysfs parent matching through `/sys/dev/char`, and `st_rdev` major/minor
//! extraction. Shared KMS policy stays in `platform::drm`.

use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{self, ErrorKind},
    os::{
        fd::{AsRawFd, BorrowedFd, OwnedFd},
        unix::fs::MetadataExt,
    },
    path::PathBuf,
};

use ::drm::{
    buffer::DrmFourcc,
    control::{
        Device as ControlDevice, Mode as DrmMode, ModeTypeFlags, PlaneType, connector, crtc,
        encoder, plane,
    },
};

use crate::{
    drm::{Device, modeset::PropMap},
    platform::drm::{
        DrmDeviceKey, DrmNode, DrmNodeKind, DrmPlatform, KmsPlatform, Mode, Output,
        open_path_cloexec,
    },
};

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

impl KmsPlatform for LinuxDrmPlatform {
    fn discover_outputs(&self, device: &Device) -> io::Result<Vec<Output>> {
        discover_outputs(device)
    }
}

fn pick_mode(modes: &[Mode]) -> Option<&Mode> {
    // Optional override: YSERVER_MODE=WxH (e.g. "1024x768") wins over the
    // kernel-reported PREFERRED mode. Useful when virtio-gpu's EDID hint
    // is ignored and the driver advertises 640x480 as preferred. Refresh
    // is matched best-effort: 60 Hz first, then any rate.
    if let Ok(spec) = std::env::var("YSERVER_MODE")
        && let Some((w, h)) = parse_mode_spec(&spec)
    {
        if let Some(m) = modes
            .iter()
            .find(|m| m.width == w && m.height == h && m.vrefresh == 60)
        {
            return Some(m);
        }
        if let Some(m) = modes.iter().find(|m| m.width == w && m.height == h) {
            return Some(m);
        }
        log::warn!(
            "YSERVER_MODE={spec} not advertised by the connector; falling back to preferred mode"
        );
    }
    if let Some(m) = modes.iter().find(|m| m.preferred) {
        return Some(m);
    }
    if let Some(m) = modes
        .iter()
        .find(|m| m.width == 1024 && m.height == 768 && m.vrefresh == 60)
    {
        return Some(m);
    }
    modes.first()
}

/// Collapse modes that are identical under our RANDR mode identity
/// `(width, height, vrefresh)`, keeping the first occurrence.
///
/// The kernel exposes the same logical resolution multiple times for a
/// connector (e.g. HDMI: an EDID detailed timing plus CEA VIC and DMT
/// entries), differing only by pixel clock / timing flags. yserver keys
/// modes solely on `(w, h, vrefresh)`, so without this collapse each of
/// those kernel duplicates is emitted as a separate `GetOutputInfo` mode
/// that all resolve to the *same* RANDR mode XID — producing the
/// `1920x1080 60.00*+ 60.00* 60.00*` triple seen in issue #48. Callers
/// pass a preferred-first list so the preferred instance survives.
fn collapse_duplicate_modes(modes: Vec<Mode>) -> Vec<Mode> {
    let mut seen: HashSet<(u16, u16, u32)> = HashSet::new();
    modes
        .into_iter()
        .filter(|m| seen.insert((m.width, m.height, m.vrefresh)))
        .collect()
}

fn parse_mode_spec(spec: &str) -> Option<(u16, u16)> {
    let (w, h) = spec.split_once('x')?;
    let w: u16 = w.trim().parse().ok()?;
    let h: u16 = h.trim().parse().ok()?;
    Some((w, h))
}

fn local_mode_from(m: &DrmMode) -> Mode {
    let (w, h) = m.size();
    // Xorg `drmmode_ConvertFromKMode`: copy kernel timing verbatim.
    let (hsync_start, hsync_end, htotal) = m.hsync();
    let (vsync_start, vsync_end, vtotal) = m.vsync();
    Mode {
        name: m.name().to_string_lossy().into_owned(),
        width: w,
        height: h,
        vrefresh: m.vrefresh(),
        preferred: m.mode_type().contains(ModeTypeFlags::PREFERRED),
        clock_khz: m.clock(),
        hsync_start,
        hsync_end,
        htotal,
        vsync_start,
        vsync_end,
        vtotal,
        flags: m.flags().bits(),
    }
}

/// One connected connector along with its candidate CRTCs and primary planes.
///
/// `candidate_planes` is each plane paired with the set of CRTCs that plane
/// can drive (i.e. the plane's `possible_crtcs` mask, already filtered to
/// `resources.crtcs()`). `assign_outputs` uses this to verify the final
/// (CRTC, plane) pairing for each connector.
struct ConnectorCandidate {
    connector: connector::Handle,
    connector_name: String,
    encoder: encoder::Handle,
    candidate_crtcs: Vec<crtc::Handle>,
    candidate_planes: Vec<(plane::Handle, HashSet<crtc::Handle>)>,
}

#[derive(Debug)]
struct Assignment {
    connector: connector::Handle,
    connector_name: String,
    // Step 3 will surface the bound encoder on `Output`; keep it on the
    // assignment so that change is local to `discover_outputs`.
    #[allow(dead_code)]
    encoder: encoder::Handle,
    crtc: crtc::Handle,
    plane: plane::Handle,
}

/// Greedy first-fit assignment of (CRTC, primary plane) pairs to connectors.
///
/// Walks `connectors` in input order. For each, picks the first
/// `candidate_crtc` not yet claimed, then the first `candidate_plane` that
/// can drive that CRTC and is not yet claimed. Returns the connector's name
/// as `Err` if no unclaimed (CRTC, plane) pair exists.
///
// TODO(phase-6.10.x): real-hardware shared encoder pools (Intel/AMD) need
// bipartite matching here — current scope is virtio-gpu where assignments
// are always disjoint.
fn assign_outputs(connectors: &[ConnectorCandidate]) -> Result<Vec<Assignment>, String> {
    let mut claimed_crtcs: HashSet<crtc::Handle> = HashSet::new();
    let mut claimed_planes: HashSet<plane::Handle> = HashSet::new();
    let mut out = Vec::with_capacity(connectors.len());

    for cand in connectors {
        let Some(&crtc) = cand
            .candidate_crtcs
            .iter()
            .find(|c| !claimed_crtcs.contains(c))
        else {
            return Err(cand.connector_name.clone());
        };
        let Some(&(plane, _)) = cand
            .candidate_planes
            .iter()
            .find(|(p, drivable)| !claimed_planes.contains(p) && drivable.contains(&crtc))
        else {
            return Err(cand.connector_name.clone());
        };
        claimed_crtcs.insert(crtc);
        claimed_planes.insert(plane);
        out.push(Assignment {
            connector: cand.connector,
            connector_name: cand.connector_name.clone(),
            encoder: cand.encoder,
            crtc,
            plane,
        });
    }

    Ok(out)
}

/// Enumerate every connected connector with usable modes and assign each
/// one a CRTC and primary plane. Greedy first-fit; see `assign_outputs`.
///
/// # Errors
/// - underlying DRM ioctls fail (resource handles, properties, etc.)
/// - a connector has no usable encoder, no candidate CRTC, or no usable
///   modes
/// - greedy assignment cannot place every connector (returns the stranded
///   connector's name in the error message)
/// - no connector is connected at all (typical when running without
///   `vng --graphics`)
///
/// # Panics
/// Panics only on internal invariant violations: a connector tracked in
/// `connector_infos` must always be present when its assignment is finalized,
/// and the picked mode must always be one of the connector's local modes.
fn discover_outputs(device: &Device) -> io::Result<Vec<Output>> {
    let resources = device.resource_handles()?;

    // Pre-collect primary planes with their possible-CRTC sets.
    // TODO(phase-6.10.x): on real hardware (Intel/AMD) primary planes are
    // shared across CRTCs and the greedy first-fit below can strand a
    // connector even though a valid assignment exists. virtio-gpu pairs
    // each plane 1:1 with a CRTC so greedy is correct for current scope.
    let mut primary_planes: Vec<(plane::Handle, HashSet<crtc::Handle>)> = Vec::new();
    for handle in device.plane_handles()? {
        let info = device.get_plane(handle)?;
        let props = device.get_properties(handle)?;
        let map = props.as_hashmap(device)?;
        let Some(type_info) = map.get("type") else {
            continue;
        };
        let raw = props
            .iter()
            .find(|(h, _)| **h == type_info.handle())
            .map(|(_, v)| *v)
            .unwrap_or(0);
        if raw != PlaneType::Primary as u64 {
            continue;
        }
        let drivable: HashSet<crtc::Handle> = resources
            .filter_crtcs(info.possible_crtcs())
            .into_iter()
            .collect();
        primary_planes.push((handle, drivable));
    }

    // Build candidates for every connected connector with usable modes.
    let mut candidates: Vec<ConnectorCandidate> = Vec::new();
    let mut connector_infos: HashMap<connector::Handle, connector::Info> = HashMap::new();
    for &handle in resources.connectors() {
        let info = device.get_connector(handle, false)?;
        if info.state() != connector::State::Connected || info.modes().is_empty() {
            continue;
        }
        let connector_name = xorg_output_name(info.interface(), info.interface_id());
        let encoder_handle = info
            .current_encoder()
            .or_else(|| info.encoders().first().copied())
            .ok_or_else(|| {
                io::Error::other(format!("connector {connector_name} has no usable encoder"))
            })?;
        let encoder_info = device.get_encoder(encoder_handle)?;
        let mut candidate_crtcs: Vec<crtc::Handle> =
            resources.filter_crtcs(encoder_info.possible_crtcs());
        // If the encoder is already bound to a CRTC, prefer it first.
        if let Some(current) = encoder_info.crtc() {
            if let Some(idx) = candidate_crtcs.iter().position(|c| *c == current) {
                candidate_crtcs.swap(0, idx);
            } else {
                candidate_crtcs.insert(0, current);
            }
        }
        if candidate_crtcs.is_empty() {
            return Err(io::Error::other(format!(
                "encoder for connector {connector_name} has no possible CRTC",
            )));
        }
        let candidate_crtc_set: HashSet<crtc::Handle> = candidate_crtcs.iter().copied().collect();
        let candidate_planes: Vec<(plane::Handle, HashSet<crtc::Handle>)> = primary_planes
            .iter()
            .filter(|(_, drivable)| drivable.iter().any(|c| candidate_crtc_set.contains(c)))
            .cloned()
            .collect();

        candidates.push(ConnectorCandidate {
            connector: handle,
            connector_name,
            encoder: encoder_handle,
            candidate_crtcs,
            candidate_planes,
        });
        connector_infos.insert(handle, info);
    }

    if candidates.is_empty() {
        return Err(io::Error::other(
            "no connected output — vng with --graphics required for modeset path; \
             headless mode does not exercise this",
        ));
    }

    let assignments = assign_outputs(&candidates).map_err(|name| {
        io::Error::other(format!(
            "connector {name} could not be placed (no unclaimed CRTC/plane)",
        ))
    })?;

    let mut outputs = Vec::with_capacity(assignments.len());
    for asg in assignments {
        let connector_info = connector_infos
            .remove(&asg.connector)
            .expect("connector_info recorded for every candidate");
        outputs.push(finalize_output(device, asg, &connector_info)?);
    }

    Ok(outputs)
}

fn finalize_output(
    device: &Device,
    asg: Assignment,
    connector_info: &connector::Info,
) -> io::Result<Output> {
    let local_modes: Vec<Mode> = connector_info.modes().iter().map(local_mode_from).collect();
    let picked = pick_mode(&local_modes)
        .ok_or_else(|| {
            io::Error::other(format!(
                "connector {} reports no usable modes",
                asg.connector_name
            ))
        })?
        .clone();
    let picked_idx = local_modes
        .iter()
        .position(|m| {
            m.name == picked.name
                && m.width == picked.width
                && m.height == picked.height
                && m.vrefresh == picked.vrefresh
        })
        .expect("picked mode is from local_modes");
    let drm_mode = connector_info.modes()[picked_idx];

    let plane_props_map = PropMap::for_object(device, asg.plane)?;
    let plane_fb_id_prop = plane_props_map.id("FB_ID")?;
    let plane_crtc_id_prop = plane_props_map.id("CRTC_ID")?;
    let plane_in_fence_fd_prop = plane_props_map.id("IN_FENCE_FD").ok();
    let crtc_out_fence_ptr_prop = PropMap::for_object(device, asg.crtc)
        .and_then(|props| props.id("OUT_FENCE_PTR"))
        .ok();
    let scanout_modifiers = plane_scanout_modifiers(device, asg.plane)?;

    log::info!(
        "yserver: connector={} crtc={:?} plane={:?} mode={} ({}x{}@{}{})",
        asg.connector_name,
        asg.crtc,
        asg.plane,
        picked.name,
        picked.width,
        picked.height,
        picked.vrefresh,
        if picked.preferred { ", preferred" } else { "" }
    );

    let (mm_width, mm_height) = connector_info.size().unwrap_or((0, 0));
    let edid = connector_edid_blob(device, asg.connector);
    let connector_type = randr_connector_type_name(&asg.connector_name);

    // Full advertised mode list, sorted preferred-first (matching Xorg
    // GetOutputInfo's nPreferred prefix). `local_modes` is kept in kernel
    // order above for the `picked_idx` -> DRM-mode mapping; we only
    // reorder this owned copy now that `drm_mode` is resolved.
    let mut modes = local_modes;
    modes.sort_by_key(|m| !m.preferred);
    let modes = collapse_duplicate_modes(modes);

    Ok(Output {
        connector: asg.connector,
        connector_name: asg.connector_name,
        crtc: asg.crtc,
        plane: asg.plane,
        mode: drm_mode,
        picked,
        plane_fb_id_prop,
        plane_crtc_id_prop,
        plane_in_fence_fd_prop,
        crtc_out_fence_ptr_prop,
        scanout_modifiers,
        mm_width,
        mm_height,
        edid,
        connector_type,
        modes,
    })
}

/// Name a connector exactly like Xorg's modesetting driver:
/// `output_names[connector_type]-connector_type_id`
/// (`hw/xfree86/drivers/modesetting/drmmode_display.c`). drm-rs's own
/// `Display`/`as_str` diverges — it renders `HDMIA` as `"HDMI-A"`,
/// giving `"HDMI-A-1"`, whereas Xorg (and therefore every X client and
/// every stored `monitors.xml` identity key) uses `"HDMI-1"`. A stored
/// GNOME/MATE monitor config keyed on `HDMI-1` never matches yserver's
/// `HDMI-A-1`, so the daemon discards the whole config and blanks the
/// desktop. Match Xorg's names verbatim.
fn xorg_output_name(interface: connector::Interface, interface_id: u32) -> String {
    use connector::Interface;
    let base = match interface {
        Interface::VGA => "VGA",
        Interface::DVII => "DVI-I",
        Interface::DVID => "DVI-D",
        Interface::DVIA => "DVI-A",
        Interface::Composite => "Composite",
        Interface::SVideo => "SVIDEO",
        Interface::LVDS => "LVDS",
        Interface::Component => "Component",
        Interface::NinePinDIN => "DIN",
        Interface::DisplayPort => "DP",
        Interface::HDMIA => "HDMI",
        Interface::HDMIB => "HDMI-B",
        Interface::TV => "TV",
        Interface::EmbeddedDisplayPort => "eDP",
        Interface::Virtual => "Virtual",
        Interface::DSI => "DSI",
        Interface::DPI => "DPI",
        // Beyond Xorg's table (newer/non-display connector types) and
        // the `#[non_exhaustive]` catch-all.
        _ => "Unknown",
    };
    format!("{base}-{interface_id}")
}

/// Read the connector's raw `EDID` property blob (empty if absent).
fn connector_edid_blob(device: &Device, connector: connector::Handle) -> Vec<u8> {
    let Ok(props) = device.get_properties(connector) else {
        return Vec::new();
    };
    for (prop_handle, raw_value) in &props {
        let Ok(info) = device.get_property(*prop_handle) else {
            continue;
        };
        if info.name().to_bytes() != b"EDID" {
            continue;
        }
        if *raw_value == 0 {
            return Vec::new();
        }
        return device.get_property_blob(*raw_value).unwrap_or_default();
    }
    Vec::new()
}

/// Map a DRM connector name (e.g. `"HDMI-A-1"`, `"DP-2"`, `"eDP-1"`) to
/// the RANDR `ConnectorType` property value name (randrproto §
/// "ConnectorType"). Best-effort; `"unknown"` when unrecognised.
fn randr_connector_type_name(connector_name: &str) -> String {
    let base = connector_name.trim();
    let ty = if base.starts_with("HDMI") {
        "HDMI"
    } else if base.starts_with("DP") || base.starts_with("DisplayPort") {
        "DisplayPort"
    } else if base.starts_with("eDP") || base.starts_with("LVDS") {
        "Panel"
    } else if base.starts_with("DVI-I") {
        "DVI-I"
    } else if base.starts_with("DVI-D") {
        "DVI-D"
    } else if base.starts_with("DVI-A") {
        "DVI-A"
    } else if base.starts_with("DVI") {
        "DVI"
    } else if base.starts_with("VGA") {
        "VGA"
    } else if base.starts_with("TV") || base.starts_with("Composite") || base.starts_with("SVIDEO")
    {
        "TV"
    } else {
        "unknown"
    };
    ty.to_string()
}

fn plane_scanout_modifiers(device: &Device, plane: plane::Handle) -> io::Result<Vec<u64>> {
    let props = device.get_properties(plane)?;
    for (prop_handle, raw_value) in &props {
        let info = device.get_property(*prop_handle)?;
        if info.name().to_bytes() != b"IN_FORMATS" {
            continue;
        }
        if *raw_value == 0 {
            return Ok(Vec::new());
        }
        let blob = device.get_property_blob(*raw_value)?;
        return Ok(parse_in_formats_modifiers(
            &blob,
            DrmFourcc::Xrgb8888 as u32,
        ));
    }
    Ok(Vec::new())
}

fn parse_in_formats_modifiers(blob: &[u8], wanted_format: u32) -> Vec<u64> {
    const HEADER_LEN: usize = 24;
    const MODIFIER_LEN: usize = 24;

    if blob.len() < HEADER_LEN {
        return Vec::new();
    }

    let read_u32 = |offset: usize| -> Option<u32> {
        let bytes: [u8; 4] = blob.get(offset..offset + 4)?.try_into().ok()?;
        Some(u32::from_ne_bytes(bytes))
    };
    let read_u64 = |offset: usize| -> Option<u64> {
        let bytes: [u8; 8] = blob.get(offset..offset + 8)?.try_into().ok()?;
        Some(u64::from_ne_bytes(bytes))
    };

    let Some(count_formats) = read_u32(8).map(|n| n as usize) else {
        return Vec::new();
    };
    let Some(formats_offset) = read_u32(12).map(|n| n as usize) else {
        return Vec::new();
    };
    let Some(count_modifiers) = read_u32(16).map(|n| n as usize) else {
        return Vec::new();
    };
    let Some(modifiers_offset) = read_u32(20).map(|n| n as usize) else {
        return Vec::new();
    };

    let Some(formats_end) = formats_offset.checked_add(count_formats.saturating_mul(4)) else {
        return Vec::new();
    };
    let Some(modifiers_end) =
        modifiers_offset.checked_add(count_modifiers.saturating_mul(MODIFIER_LEN))
    else {
        return Vec::new();
    };
    if formats_end > blob.len() || modifiers_end > blob.len() {
        return Vec::new();
    }

    let mut formats = Vec::with_capacity(count_formats);
    for i in 0..count_formats {
        let Some(format) = read_u32(formats_offset + i * 4) else {
            return Vec::new();
        };
        formats.push(format);
    }

    let mut modifiers = Vec::new();
    for i in 0..count_modifiers {
        let base = modifiers_offset + i * MODIFIER_LEN;
        let Some(format_bits) = read_u64(base) else {
            return Vec::new();
        };
        let Some(offset) = read_u32(base + 8) else {
            return Vec::new();
        };
        let Some(modifier) = read_u64(base + 16) else {
            return Vec::new();
        };
        let offset = offset as usize;
        for bit in 0..64 {
            if (format_bits & (1_u64 << bit)) == 0 {
                continue;
            }
            let idx = offset + bit;
            if formats.get(idx).copied() == Some(wanted_format) && !modifiers.contains(&modifier) {
                modifiers.push(modifier);
            }
        }
    }
    modifiers
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

    #[test]
    fn xorg_output_name_matches_modesetting_driver() {
        use ::drm::control::connector::Interface;
        // The bug: HDMI-A must render as "HDMI-1" (Xorg), not "HDMI-A-1"
        // (drm-rs). A stored monitors.xml keyed on HDMI-1 depends on this.
        assert_eq!(xorg_output_name(Interface::HDMIA, 1), "HDMI-1");
        assert_eq!(xorg_output_name(Interface::DisplayPort, 1), "DP-1");
        assert_eq!(xorg_output_name(Interface::DVII, 2), "DVI-I-2");
        assert_eq!(xorg_output_name(Interface::EmbeddedDisplayPort, 1), "eDP-1");
        assert_eq!(xorg_output_name(Interface::VGA, 1), "VGA-1");
    }

    #[test]
    fn randr_connector_type_name_maps_drm_connector_names() {
        assert_eq!(randr_connector_type_name("HDMI-A-1"), "HDMI");
        assert_eq!(randr_connector_type_name("HDMI-B-2"), "HDMI");
        assert_eq!(randr_connector_type_name("DP-1"), "DisplayPort");
        assert_eq!(randr_connector_type_name("DisplayPort-0"), "DisplayPort");
        assert_eq!(randr_connector_type_name("eDP-1"), "Panel");
        assert_eq!(randr_connector_type_name("LVDS-1"), "Panel");
        assert_eq!(randr_connector_type_name("DVI-I-1"), "DVI-I");
        assert_eq!(randr_connector_type_name("DVI-D-1"), "DVI-D");
        assert_eq!(randr_connector_type_name("VGA-1"), "VGA");
        assert_eq!(randr_connector_type_name("Virtual-1"), "unknown");
    }

    fn mode(name: &str, w: u16, h: u16, refresh: u32, preferred: bool) -> Mode {
        Mode {
            name: name.into(),
            width: w,
            height: h,
            vrefresh: refresh,
            preferred,
            ..Default::default()
        }
    }

    #[test]
    fn picks_preferred_when_present() {
        let modes = vec![
            mode("800x600", 800, 600, 60, false),
            mode("1024x768", 1024, 768, 60, true),
            mode("1920x1080", 1920, 1080, 60, false),
        ];
        let picked = pick_mode(&modes).unwrap();
        assert_eq!(picked.name, "1024x768");
    }

    #[test]
    fn falls_back_to_1024x768_60_when_no_preferred() {
        let modes = vec![
            mode("800x600", 800, 600, 60, false),
            mode("1024x768", 1024, 768, 60, false),
            mode("1920x1080", 1920, 1080, 60, false),
        ];
        let picked = pick_mode(&modes).unwrap();
        assert_eq!(picked.name, "1024x768");
    }

    #[test]
    fn falls_back_to_first_when_no_preferred_and_no_1024x768() {
        let modes = vec![
            mode("800x600", 800, 600, 60, false),
            mode("1920x1080", 1920, 1080, 60, false),
        ];
        let picked = pick_mode(&modes).unwrap();
        assert_eq!(picked.name, "800x600");
    }

    #[test]
    fn empty_list_returns_none() {
        assert!(pick_mode(&[]).is_none());
    }

    #[test]
    fn collapse_duplicate_modes_dedups_by_resolution_keeping_first() {
        // HDMI displays routinely expose the same wxh@refresh several
        // times (EDID detailed timing + CEA VIC + DMT), differing only by
        // pixel clock/flags - all collapse to one (w,h,vrefresh) in our
        // RANDR model. Issue #48: this leaked three identical 1920x1080@60
        // XIDs into GetOutputInfo (`xrandr` showed `60.00*+ 60.00* 60.00*`).
        // Input is preferred-first (as in finalize_output): the preferred
        // instance leads, so first-occurrence-wins keeps it.
        let modes = vec![
            mode("3440x1440", 3440, 1440, 165, true),
            mode("1920x1080-edid", 1920, 1080, 60, true),
            mode("1920x1080-cea", 1920, 1080, 60, false),
            mode("1920x1080-dmt", 1920, 1080, 60, false),
        ];
        let deduped = collapse_duplicate_modes(modes);

        assert_eq!(deduped.len(), 2, "two unique (w,h,vrefresh) modes remain");
        assert_eq!(deduped[0].name, "3440x1440", "order preserved");
        assert_eq!(deduped[1].name, "1920x1080-edid", "first occurrence wins");
        assert!(deduped[1].preferred, "the preferred instance survives");
    }

    use ::drm::control::from_u32;

    fn ch(n: u32) -> connector::Handle {
        from_u32(n).expect("non-zero raw handle")
    }
    fn eh(n: u32) -> encoder::Handle {
        from_u32(n).expect("non-zero raw handle")
    }
    fn rh(n: u32) -> crtc::Handle {
        from_u32(n).expect("non-zero raw handle")
    }
    fn ph(n: u32) -> plane::Handle {
        from_u32(n).expect("non-zero raw handle")
    }

    fn cand(
        idx: u32,
        name: &str,
        crtcs: Vec<crtc::Handle>,
        planes: Vec<(plane::Handle, &[crtc::Handle])>,
    ) -> ConnectorCandidate {
        ConnectorCandidate {
            connector: ch(idx),
            connector_name: name.into(),
            encoder: eh(idx),
            candidate_crtcs: crtcs,
            candidate_planes: planes
                .into_iter()
                .map(|(p, cs)| (p, cs.iter().copied().collect()))
                .collect(),
        }
    }

    #[test]
    fn assigns_two_connectors_with_disjoint_crtcs_in_input_order() {
        let c0 = rh(10);
        let c1 = rh(11);
        let p0 = ph(20);
        let p1 = ph(21);
        let cands = vec![
            cand(1, "HDMI-1", vec![c0], vec![(p0, &[c0])]),
            cand(2, "HDMI-2", vec![c1], vec![(p1, &[c1])]),
        ];
        let asg = assign_outputs(&cands).expect("assignment succeeds");
        assert_eq!(asg.len(), 2);
        assert_eq!(asg[0].connector_name, "HDMI-1");
        assert_eq!(asg[0].crtc, c0);
        assert_eq!(asg[0].plane, p0);
        assert_eq!(asg[1].connector_name, "HDMI-2");
        assert_eq!(asg[1].crtc, c1);
        assert_eq!(asg[1].plane, p1);
    }

    #[test]
    fn errors_when_connector_has_no_candidate_crtcs() {
        let cands = vec![cand(1, "HDMI-stranded", vec![], vec![])];
        let err = assign_outputs(&cands).expect_err("must error");
        assert_eq!(err, "HDMI-stranded");
    }

    #[test]
    fn errors_on_second_connector_when_one_crtc_shared() {
        let c0 = rh(10);
        let p0 = ph(20);
        let p1 = ph(21);
        let cands = vec![
            cand(1, "HDMI-A", vec![c0], vec![(p0, &[c0])]),
            cand(2, "HDMI-B", vec![c0], vec![(p1, &[c0])]),
        ];
        let err = assign_outputs(&cands).expect_err("must error");
        assert_eq!(err, "HDMI-B");
    }

    #[test]
    fn errors_when_no_plane_can_drive_candidate_crtcs() {
        let c0 = rh(10);
        let c_other = rh(99);
        let p0 = ph(20);
        // plane only drives c_other, which is not a candidate.
        let cands = vec![cand(1, "HDMI-NoPlane", vec![c0], vec![(p0, &[c_other])])];
        let err = assign_outputs(&cands).expect_err("must error");
        assert_eq!(err, "HDMI-NoPlane");
    }

    #[test]
    fn parses_in_formats_modifiers_for_xrgb8888() {
        let mut blob = Vec::new();
        let formats = [0x1111_1111, DrmFourcc::Xrgb8888 as u32];
        let formats_offset = 24_u32;
        let modifiers_offset = 32_u32;
        blob.extend_from_slice(&1_u32.to_ne_bytes()); // version
        blob.extend_from_slice(&0_u32.to_ne_bytes()); // flags
        blob.extend_from_slice(&(formats.len() as u32).to_ne_bytes());
        blob.extend_from_slice(&formats_offset.to_ne_bytes());
        blob.extend_from_slice(&2_u32.to_ne_bytes()); // count_modifiers
        blob.extend_from_slice(&modifiers_offset.to_ne_bytes());
        for format in formats {
            blob.extend_from_slice(&format.to_ne_bytes());
        }

        // Modifier 0 applies to format index 0 only; modifier 1 applies
        // to format index 1 (XRGB8888).
        blob.extend_from_slice(&1_u64.to_ne_bytes()); // formats bitset
        blob.extend_from_slice(&0_u32.to_ne_bytes()); // offset
        blob.extend_from_slice(&0_u32.to_ne_bytes()); // pad
        blob.extend_from_slice(&0xaaaa_u64.to_ne_bytes());
        blob.extend_from_slice(&(1_u64 << 1).to_ne_bytes());
        blob.extend_from_slice(&0_u32.to_ne_bytes());
        blob.extend_from_slice(&0_u32.to_ne_bytes());
        blob.extend_from_slice(&0xbbbb_u64.to_ne_bytes());

        let modifiers = parse_in_formats_modifiers(&blob, DrmFourcc::Xrgb8888 as u32);
        assert_eq!(modifiers, vec![0xbbbb]);
    }
}
