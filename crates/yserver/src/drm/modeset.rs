use std::{collections::HashMap, io};

use drm::control::{
    AtomicCommitFlags, Device as ControlDevice, atomic::AtomicModeReq, framebuffer, property,
};

use crate::{drm::Device, platform::drm::Output};

#[allow(dead_code)]
pub(crate) fn dump_properties(device: &Device, output: &Output) -> io::Result<()> {
    log::debug!("=== connector {} properties ===", output.connector_name);
    dump_object_properties(device, output.connector)?;
    log::debug!("=== crtc {:?} properties ===", output.crtc);
    dump_object_properties(device, output.crtc)?;
    log::debug!("=== plane {:?} properties ===", output.plane);
    dump_object_properties(device, output.plane)?;
    Ok(())
}

#[allow(dead_code)]
fn dump_object_properties<H>(device: &Device, handle: H) -> io::Result<()>
where
    H: drm::control::ResourceHandle,
{
    let props = device.get_properties(handle)?;
    for (prop_handle, raw_value) in &props {
        let info = device.get_property(*prop_handle)?;
        log::debug!(
            "  {} = 0x{:x} ({:?})",
            info.name().to_string_lossy(),
            raw_value,
            info.value_type()
        );
    }
    Ok(())
}

pub(crate) struct PropMap {
    handles: HashMap<String, property::Info>,
}

impl PropMap {
    pub(crate) fn for_object<H>(device: &Device, handle: H) -> io::Result<Self>
    where
        H: drm::control::ResourceHandle,
    {
        let props = device.get_properties(handle)?;
        Ok(Self {
            handles: props.as_hashmap(device)?,
        })
    }

    pub(crate) fn id(&self, name: &str) -> io::Result<property::Handle> {
        self.handles
            .get(name)
            .map(|info| info.handle())
            .ok_or_else(|| io::Error::other(format!("property {name:?} not exposed")))
    }
}

pub(crate) fn disable_output(device: &Device, output: &Output) -> io::Result<()> {
    let connector_props = PropMap::for_object(device, output.connector)?;
    let crtc_props = PropMap::for_object(device, output.crtc)?;

    let mut req = AtomicModeReq::new();
    req.add_raw_property(output.plane.into(), output.plane_fb_id_prop, 0);
    req.add_raw_property(output.plane.into(), output.plane_crtc_id_prop, 0);
    req.add_raw_property(output.crtc.into(), crtc_props.id("ACTIVE")?, 0);
    req.add_raw_property(output.crtc.into(), crtc_props.id("MODE_ID")?, 0);
    req.add_raw_property(output.connector.into(), connector_props.id("CRTC_ID")?, 0);

    device
        .atomic_commit(AtomicCommitFlags::ALLOW_MODESET, req)
        .map_err(|err| {
            io::Error::new(
                err.kind(),
                format!("disable_output atomic commit rejected: {err}"),
            )
        })
}

pub(crate) fn commit_modeset(
    device: &Device,
    output: &Output,
    fb_id: framebuffer::Handle,
) -> io::Result<()> {
    modeset_with_flags(
        device,
        output,
        fb_id,
        AtomicCommitFlags::ALLOW_MODESET,
        "atomic modeset commit",
    )
}

/// Validate a complete modeset without changing the hardware state.
pub(crate) fn test_modeset(
    device: &Device,
    output: &Output,
    fb_id: framebuffer::Handle,
) -> io::Result<()> {
    modeset_with_flags(
        device,
        output,
        fb_id,
        AtomicCommitFlags::ALLOW_MODESET | AtomicCommitFlags::TEST_ONLY,
        "atomic modeset TEST_ONLY",
    )
}

fn modeset_with_flags(
    device: &Device,
    output: &Output,
    fb_id: framebuffer::Handle,
    flags: AtomicCommitFlags,
    operation: &str,
) -> io::Result<()> {
    let connector_props = PropMap::for_object(device, output.connector)?;
    let crtc_props = PropMap::for_object(device, output.crtc)?;
    let plane_props = PropMap::for_object(device, output.plane)?;

    let mode_blob = device.create_property_blob(&output.mode)?;
    let mode_blob_raw: u64 = mode_blob.into();

    let crtc_id_raw: u32 = output.crtc.into();
    let plane_crtc_raw: u32 = output.crtc.into();
    let fb_id_raw: u32 = fb_id.into();
    let (mode_w, mode_h) = output.mode.size();
    let src_w = u64::from(mode_w) << 16;
    let src_h = u64::from(mode_h) << 16;

    let mut req = AtomicModeReq::new();
    req.add_raw_property(
        output.connector.into(),
        connector_props.id("CRTC_ID")?,
        u64::from(crtc_id_raw),
    );
    req.add_raw_property(output.crtc.into(), crtc_props.id("MODE_ID")?, mode_blob_raw);
    req.add_raw_property(output.crtc.into(), crtc_props.id("ACTIVE")?, 1);
    req.add_raw_property(
        output.plane.into(),
        plane_props.id("FB_ID")?,
        u64::from(fb_id_raw),
    );
    req.add_raw_property(
        output.plane.into(),
        plane_props.id("CRTC_ID")?,
        u64::from(plane_crtc_raw),
    );
    req.add_raw_property(output.plane.into(), plane_props.id("SRC_X")?, 0);
    req.add_raw_property(output.plane.into(), plane_props.id("SRC_Y")?, 0);
    req.add_raw_property(output.plane.into(), plane_props.id("SRC_W")?, src_w);
    req.add_raw_property(output.plane.into(), plane_props.id("SRC_H")?, src_h);
    req.add_raw_property(output.plane.into(), plane_props.id("CRTC_X")?, 0);
    req.add_raw_property(output.plane.into(), plane_props.id("CRTC_Y")?, 0);
    req.add_raw_property(
        output.plane.into(),
        plane_props.id("CRTC_W")?,
        u64::from(mode_w),
    );
    req.add_raw_property(
        output.plane.into(),
        plane_props.id("CRTC_H")?,
        u64::from(mode_h),
    );

    let result = device.atomic_commit(flags, req);
    let _ = device.destroy_property_blob(mode_blob_raw);
    result.map_err(|err| {
        io::Error::new(
            err.kind(),
            format!(
                "{operation} rejected (mode {}, {}x{}): {err}",
                output.picked.name, output.picked.width, output.picked.height
            ),
        )
    })
}
