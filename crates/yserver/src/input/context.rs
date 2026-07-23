//! libinput context wrapper.
//!
//! Owns an `input::Libinput` against udev seat0 with a `LibinputInterface`
//! that honours the flags libinput requests (per the libinput contract —
//! some devices are read-only, forcing O_RDWR breaks them). The context
//! exposes its fd for epoll integration and a `dispatch()` method that
//! pulls pending libinput events and translates the relevant subset to
//! [`InputEvent`].

use std::{
    collections::{HashMap, HashSet},
    fs::{File, OpenOptions},
    io,
    os::{
        fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd},
        unix::fs::OpenOptionsExt,
    },
    path::Path,
};

use input::{
    Device, DeviceCapability, Event, Led, Libinput, LibinputInterface,
    event::{
        EventTrait,
        keyboard::{KeyState, KeyboardEvent, KeyboardEventTrait},
        pointer::{Axis, ButtonState, PointerEvent, PointerScrollEvent},
    },
};
use libc::{O_ACCMODE, O_RDONLY, O_RDWR, O_WRONLY};
use yserver_core::{
    core_loop::{
        DeviceInfo,
        message::{LibinputConfigSnapshot, device_node_from_sysname},
    },
    xinput::libinput_props::{DeviceConfigChange, DeviceConfigError},
};

use crate::input::{event::InputEvent, libinput_config};

struct Interface;

impl LibinputInterface for Interface {
    fn open_restricted(&mut self, path: &Path, flags: i32) -> Result<OwnedFd, i32> {
        let result = OpenOptions::new()
            .custom_flags(flags)
            .read((flags & O_ACCMODE == O_RDONLY) | (flags & O_ACCMODE == O_RDWR))
            .write((flags & O_ACCMODE == O_WRONLY) | (flags & O_ACCMODE == O_RDWR))
            .open(path);
        match result {
            Ok(file) => {
                log::debug!("libinput: open_restricted ok: {}", path.display());
                Ok(file.into())
            }
            Err(err) => {
                log::warn!(
                    "libinput: open_restricted failed: {} -> {err}",
                    path.display()
                );
                Err(err.raw_os_error().unwrap_or(libc::EIO))
            }
        }
    }

    fn close_restricted(&mut self, fd: OwnedFd) {
        drop(File::from(fd));
    }
}

pub struct Context {
    libinput: Libinput,
    /// Live libinput device handles keyed by evdev devnode (e.g.
    /// `/dev/input/event4`). Populated at `DeviceAdded` for any pointer
    /// device (touchpad OR mouse — the ones whose libinput properties we
    /// seed); cleared at `DeviceRemoved`. Consumed by
    /// [`Context::apply_device_config`] so decoded `xinput set-prop`
    /// writes (e.g. the KDE Mouse KCM setting `libinput Accel Speed`) can
    /// be routed through to the matching `config_*_set_*` setter on the
    /// live device.
    ///
    /// `input::Device` is refcounted at the C level (`libinput_device_ref`)
    /// and the Rust wrapper exposes that via `Clone` — stashing the handle
    /// here keeps the device alive even after libinput's own iterator
    /// drops its borrow, and the entry's eventual `remove(...)` drops
    /// the last ref.
    pointer_devices: HashMap<String, Device>,
    /// Live handles for keyboard-capability devices, same keying and
    /// refcount semantics as `pointer_devices`. Consumed by
    /// [`Context::update_leds`] — the XKB lock state (Caps/Num/Scroll)
    /// lives in the server core, so the server must push LED changes
    /// down to the hardware via `libinput_device_led_update`; nothing
    /// else will (this is the KMS server, there is no other driver).
    keyboard_devices: HashMap<String, Device>,
    /// Last LED mask applied — re-applied to keyboards that appear
    /// later (hotplug, VT-switch re-acquire re-adds devices with their
    /// LEDs reset).
    last_leds: Led,
    /// Device nodes of currently-open **keyboard- or pointer-capable**
    /// devices. The startup guard requires this to be non-empty: a session
    /// whose only opened device is non-usable (e.g. a lone HID "System
    /// Control" collection that opened while the real keyboard/mouse were
    /// seat-denied) is dead on arrival and can't even be zapped. Add/remove
    /// tracked so the count stays accurate across hotplug.
    usable_input_nodes: HashSet<String>,
}

/// Newtype wrapper around `Context` that implements `Send`.
/// SAFETY: The libinput thread is the sole owner. We need `Send` only
/// because the context crosses the spawn boundary into that thread.
pub struct SendContext(Context);
unsafe impl Send for SendContext {}

impl SendContext {
    pub fn new() -> io::Result<Self> {
        Context::new().map(Self)
    }

    pub fn fd(&self) -> RawFd {
        self.0.fd()
    }

    pub fn dispatch(&mut self) -> io::Result<Vec<InputEvent>> {
        self.0.dispatch()
    }

    /// Number of open keyboard/pointer-capable devices (startup guard).
    pub fn usable_input_device_count(&self) -> usize {
        self.0.usable_input_device_count()
    }

    pub fn update_leds(&mut self, leds: Led) {
        self.0.update_leds(leds);
    }

    pub fn suspend(&mut self) {
        self.0.suspend();
    }

    pub fn resume(&mut self) -> io::Result<()> {
        self.0.resume()
    }

    /// Route a `xinput set-prop` write to the wrapped libinput context.
    /// The input thread owns the live device map, so client device-config
    /// writes (forwarded over `InputThreadControl`) land via this forward.
    ///
    /// # Errors
    ///
    /// Propagates libinput's [`DeviceConfigError`] (Unsupported / Invalid)
    /// from the inner [`Context::apply_device_config`].
    pub fn apply_device_config(
        &mut self,
        device_node: &str,
        change: DeviceConfigChange,
    ) -> Result<(), DeviceConfigError> {
        self.0.apply_device_config(device_node, change)
    }
}

impl AsFd for SendContext {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl Context {
    pub fn new() -> io::Result<Self> {
        // Access check (always-Direct: no libseat to grant device access).
        // If input nodes exist but any is permission-denied, yserver lacks
        // input access — the real keyboard/mouse won't open even if an odd
        // node does. Fail here so startup refuses (routed via the no-input
        // abort) instead of coming up with a dead, un-zappable session.
        let (present, permission_denied) = probe_input_devnodes();
        if present > 0 && permission_denied > 0 {
            return Err(io::Error::other(format!(
                "cannot open input devices: {permission_denied} of {present} \
                 /dev/input/event* nodes are permission-denied.\n\
                 yserver has no seat/libseat; it needs direct access to input \
                 devices — add the user to the 'input' group (or grant the \
                 seat ACL) and run from the console, not over SSH."
            )));
        }
        let mut libinput = Libinput::new_with_udev(Interface);
        libinput.udev_assign_seat("seat0").map_err(|()| {
            io::Error::other(
                "libinput: udev_assign_seat(\"seat0\") failed — is udev running and the \
                 seat reachable from this process?",
            )
        })?;
        Ok(Self {
            libinput,
            pointer_devices: HashMap::new(),
            keyboard_devices: HashMap::new(),
            last_leds: Led::empty(),
            usable_input_nodes: HashSet::new(),
        })
    }

    pub fn fd(&self) -> RawFd {
        self.libinput.as_raw_fd()
    }

    /// Count of currently-open **keyboard- or pointer-capable** devices.
    /// The startup guard requires this to be ≥1 — a session with input
    /// devices that are none of keyboard/pointer (e.g. only a HID "System
    /// Control" node) is unusable. See [`usable_input_nodes`](Self).
    pub fn usable_input_device_count(&self) -> usize {
        self.usable_input_nodes.len()
    }

    pub fn dispatch(&mut self) -> io::Result<Vec<InputEvent>> {
        self.libinput.dispatch()?;
        let mut out = Vec::new();
        for event in &mut self.libinput {
            // Log device add/remove unconditionally so we can tell from
            // the server log whether libinput is seeing input hardware.
            // No devices ever logged → seat permission / udev issue.
            match &event {
                Event::Device(input::event::DeviceEvent::Added(d)) => {
                    let mut dev = d.device();
                    let name = dev.name().into_owned();
                    let tap_finger_count = dev.config_tap_finger_count();
                    let is_tp = is_touchpad(tap_finger_count);
                    if is_tp {
                        configure_touchpad(&mut dev, &name);
                        log::info!(
                            "libinput: device added: {name:?} (touchpad — tap-to-click + \
                             disable-while-typing enabled)"
                        );
                    } else {
                        log::info!("libinput: device added: {name:?}");
                    }
                    let sysname = dev.sysname().to_owned();
                    // Prefer the real udev devnode; fall back to the
                    // derived path (libinput sysname == `eventN`, so
                    // the node is always `/dev/input/eventN`).
                    let device_node = {
                        // SAFETY: libinput holds the udev device alive
                        // for the duration of this event; we only read
                        // the devnode string and drop the handle.
                        let node = unsafe { dev.udev_device() }
                            .and_then(|ud| ud.devnode().map(|p| p.to_string_lossy().into_owned()));
                        node.unwrap_or_else(|| device_node_from_sysname(&sysname))
                    };
                    // T4: gather the live config snapshot for any pointer
                    // device (touchpad OR mouse) so the XI2 property registry
                    // exposes which libinput knobs are available / current /
                    // default on it. A mouse still has accel / left-handed /
                    // natural-scroll / send-events knobs — the KDE Mouse KCM
                    // reads `libinput Accel Speed` and SIGSEGVs if the atom is
                    // absent. Non-pointer devices (keyboards) keep the
                    // all-`false` default snapshot.
                    let is_pointer = dev.has_capability(DeviceCapability::Pointer);
                    let config = if is_tp || is_pointer {
                        libinput_config::gather(&dev)
                    } else {
                        LibinputConfigSnapshot::default()
                    };
                    // A *real* relative pointer reports pointer acceleration.
                    // The phantom HID "Consumer Control" / "System Control"
                    // collections a keyboard or wireless receiver exposes are
                    // pointer-capable to libinput but have no accel; excluding
                    // them here keeps them from clobbering the single
                    // slave-pointer slot (id 4, latest-wins) that the real
                    // mouse must own, and keeps the write map to real devices.
                    let is_real_pointer = is_tp || config.accel.available;
                    // Stash the live device handle keyed by devnode so
                    // `apply_device_config` (the KCM XIChangeProperty write
                    // path) can route to the `config_*_set_*` setter.
                    // `Device: Clone` is libinput's C-level refcount bump
                    // (`libinput_device_ref`), so the handle survives after
                    // this event's borrow drops. Stored for every real pointer
                    // we seed props for — otherwise a mouse's write (accel,
                    // left-handed, …) would silently no-op.
                    if is_real_pointer {
                        self.pointer_devices
                            .insert(device_node.clone(), dev.clone());
                    }
                    // Keyboard-capability devices are stashed for LED
                    // writes (update_leds). Re-apply the current lock-
                    // LED mask to a newly-appearing keyboard: hotplug
                    // and VT-switch re-acquire re-add devices with
                    // their LEDs reset, but the X-side lock state
                    // persists.
                    if dev.has_capability(DeviceCapability::Keyboard) {
                        // Force the device to the current lock state
                        // unconditionally — including all-off — so a
                        // keyboard that appears with a stale firmware
                        // LED (e.g. a BIOS-lit NumLock) is corrected to
                        // match the server, not just keyboards added
                        // while a lock happens to be active.
                        dev.led_update(self.last_leds);
                        self.keyboard_devices
                            .insert(device_node.clone(), dev.clone());
                    }
                    // Track keyboard/pointer-capable devices for the startup
                    // usable-input guard (`usable_input_device_count`). A lone
                    // non-usable device (e.g. a HID "System Control" collection
                    // that opens while the real keyboard/mouse are seat-denied)
                    // must NOT count as usable input.
                    if dev.has_capability(DeviceCapability::Keyboard)
                        || dev.has_capability(DeviceCapability::Pointer)
                    {
                        self.usable_input_nodes.insert(device_node.clone());
                    }
                    let info = DeviceInfo {
                        name,
                        device_node,
                        sysname,
                        vendor_id: dev.id_vendor(),
                        product_id: dev.id_product(),
                        is_touchpad: is_tp,
                        config,
                    };
                    out.push(InputEvent::DeviceAdded(info));
                }
                Event::Device(input::event::DeviceEvent::Removed(d)) => {
                    let dev = d.device();
                    let name = dev.name();
                    log::info!("libinput: device removed: {name:?}");
                    let sysname = dev.sysname().to_owned();
                    let device_node = {
                        let node = unsafe { dev.udev_device() }
                            .and_then(|ud| ud.devnode().map(|p| p.to_string_lossy().into_owned()));
                        node.unwrap_or_else(|| device_node_from_sysname(&sysname))
                    };
                    // T4: drop the stashed handle (libinput unref via Drop).
                    // No-op if the device wasn't a touchpad (never inserted).
                    self.pointer_devices.remove(&device_node);
                    self.keyboard_devices.remove(&device_node);
                    self.usable_input_nodes.remove(&device_node);
                    out.push(InputEvent::DeviceRemoved { device_node });
                }
                _ => {}
            }
            if let Some(translated) = translate(&event) {
                out.push(translated);
            }
        }
        Ok(out)
    }

    /// Push the X-side lock-LED state (Caps/Num/Scroll) to every keyboard
    /// device. Called from the input thread after a [`crate::input::LedRelay`]
    /// wakeup. Also remembered for keyboards that appear later (see the
    /// DeviceAdded arm).
    pub fn update_leds(&mut self, leds: Led) {
        self.last_leds = leds;
        for dev in self.keyboard_devices.values_mut() {
            dev.led_update(leds);
        }
    }

    /// Route a decoded `xinput set-prop` / KCM `XIChangeProperty` write
    /// through to the live libinput device it targets, keyed by the XI
    /// device's `device_node`. Config is per-device (as on Xorg, where each
    /// physical pointer is its own XI slave): a write to the mouse must not
    /// touch a touchpad, so this applies ONLY to the matching device.
    ///
    /// LIMITATION: yserver currently exposes a single slave-pointer entry
    /// (id 4) that whichever real pointer seeded last owns, so only that one
    /// device is configurable per-session. The correct long-term fix is to
    /// expose one XI slave per physical pointer like Xorg
    /// (see `project_kcm_mouse_crash_libinput_accel`).
    ///
    /// Returns `Ok(())` when no matching device is stashed for
    /// `device_node` (an unplugged / non-real-pointer target is a no-op; the
    /// property registry stays writable and the X11 reply path doesn't
    /// surface a "device gone" error to clients).
    ///
    /// Errors map libinput's [`input::DeviceConfigError`] onto the
    /// X-layer's [`DeviceConfigError`]: `Unsupported` → BadMatch,
    /// `Invalid` → BadValue (the mapping is performed by the
    /// `dispatch_change_property` helper that calls us).
    ///
    /// # Errors
    ///
    /// Returns [`DeviceConfigError::Unsupported`] when libinput
    /// reports the setting isn't available on this device, or
    /// [`DeviceConfigError::Invalid`] when the value is out of range.
    pub fn apply_device_config(
        &mut self,
        device_node: &str,
        change: DeviceConfigChange,
    ) -> Result<(), DeviceConfigError> {
        self.pointer_devices
            .get_mut(device_node)
            .map_or(Ok(()), |dev| libinput_config::apply(dev, change))
    }
}

impl AsFd for Context {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.libinput.as_fd()
    }
}

/// A libinput device is a touchpad iff it reports a tap finger count.
/// libinput/wlroots classify touchpads this way: pointers that are not
/// touchpads (mice, trackpoints) report a finger count of 0, while
/// clickpads/touchpads report >= 1. We use this to decide whether to
/// apply touchpad-friendly defaults at device-add time.
fn is_touchpad(tap_finger_count: u32) -> bool {
    tap_finger_count > 0
}

/// Apply touchpad-friendly defaults at device-add so the laptop is
/// usable without a settings daemon. libinput defaults tapping OFF, so
/// without this "tap to click" does nothing on a fresh yserver session
/// (the reported yoga symptom). We also enable disable-while-typing to
/// suppress accidental cursor jumps while typing. Scroll direction is
/// left at the libinput default to avoid surprising the user by
/// reversing it. Errors are logged, not fatal — a touchpad that rejects
/// a config still works, just without that nicety.
fn configure_touchpad(dev: &mut Device, name: &str) {
    if let Err(e) = dev.config_tap_set_enabled(true) {
        log::warn!("libinput: enable tap-to-click on {name:?} failed: {e:?}");
    }
    if let Err(e) = dev.config_dwt_set_enabled(true) {
        // Many touchpads don't support DWT; that's expected, so debug.
        log::debug!("libinput: disable-while-typing on {name:?} unavailable: {e:?}");
    }
}

impl Context {
    /// Suspend libinput: closes all open input device fds. The context remains
    /// valid and can be resumed with [`Context::resume`].
    ///
    /// `pointer_devices` is intentionally left as-is — the stashed
    /// handles point at devices whose fds are now closed, but
    /// libinput's own `DeviceRemoved` (or a fresh `DeviceAdded` after
    /// `resume`) will replace each entry the next time `dispatch()`
    /// runs. Any `apply_device_config` write that races against an
    /// open suspend window targets a closed device and returns
    /// libinput's UNSUPPORTED — caller-visible as BadMatch.
    pub fn suspend(&mut self) {
        self.libinput.suspend();
    }

    /// Resume a suspended libinput context. Re-enables device monitoring and
    /// re-opens devices via `open_restricted`.
    ///
    /// # Errors
    ///
    /// Returns `Err` if `libinput_resume` returns -1.
    pub fn resume(&mut self) -> io::Result<()> {
        self.libinput
            .resume()
            .map_err(|()| io::Error::other("libinput resume failed"))
    }
}

/// Best-effort `/dev/input/` enumeration logged at startup. Lets us
/// tell from the log whether the input nodes exist and whether our
/// process can stat / open them. udev rules from logind grant ACL on
/// `event*` to the active session; if we see `open: ok` here but
/// libinput's `open_restricted` fails, the seat is the wrong one.
/// Probe every `/dev/input/event*` node with an `O_RDONLY` open, logging
/// each result, and return `(present, permission_denied)`.
///
/// This is the access check that matters now that yserver is always Direct
/// (no libseat): a session with input access (in the `input` group / holding
/// the seat's ACL) can open every input node. Any `EACCES`/`EPERM` here means
/// yserver does NOT have input access — the real keyboard/mouse won't work even
/// if some odd node (e.g. a HID "System Control" collection, which libinput
/// still reports as keyboard-capable) happens to open.
fn probe_input_devnodes() -> (usize, usize) {
    let dir = match std::fs::read_dir("/dev/input") {
        Ok(d) => d,
        Err(err) => {
            log::warn!("/dev/input: read_dir failed: {err}");
            return (0, 0);
        }
    };
    let mut nodes: Vec<_> = dir.flatten().collect();
    nodes.sort_by_key(std::fs::DirEntry::file_name);
    let mut present = 0usize;
    let mut permission_denied = 0usize;
    for entry in nodes {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if !name_str.starts_with("event") {
            continue;
        }
        present += 1;
        let path = entry.path();
        match OpenOptions::new().read(true).open(&path) {
            Ok(_f) => log::debug!("/dev/input/{name_str}: open(O_RDONLY) ok"),
            Err(err) => {
                if matches!(err.raw_os_error(), Some(libc::EACCES | libc::EPERM)) {
                    permission_denied += 1;
                }
                log::warn!("/dev/input/{name_str}: open(O_RDONLY) failed: {err}");
            }
        }
    }
    (present, permission_denied)
}

/// Finger/continuous scroll → `PointerScroll` v120 quantization.
/// Both event types expose only `scroll_value` (in cursor-pixel-
/// equivalent units, no v120 quantization). Convert at ~15 px per
/// logical wheel click (xwayland/Sway convention) → factor 8.
///
/// `has_axis(axis)` MUST be checked first: libinput emits a
/// `client bug: value requested for unset axis` error if
/// `scroll_value` is called for an axis the event doesn't carry.
fn finger_or_continuous_to_event<E>(ev: &E) -> Option<InputEvent>
where
    E: PointerScrollEvent,
{
    const PX_TO_V120: f64 = 8.0;
    let dx_v120 = if ev.has_axis(Axis::Horizontal) {
        (ev.scroll_value(Axis::Horizontal) * PX_TO_V120) as i32
    } else {
        0
    };
    let dy_v120 = if ev.has_axis(Axis::Vertical) {
        (ev.scroll_value(Axis::Vertical) * PX_TO_V120) as i32
    } else {
        0
    };
    if dx_v120 == 0 && dy_v120 == 0 {
        return None;
    }
    Some(InputEvent::PointerScroll { dx_v120, dy_v120 })
}

fn translate(event: &Event) -> Option<InputEvent> {
    match event {
        Event::Keyboard(KeyboardEvent::Key(key)) => {
            let keycode = key.key();
            Some(match key.key_state() {
                KeyState::Pressed => InputEvent::KeyPress { keycode },
                KeyState::Released => InputEvent::KeyRelease { keycode },
            })
        }
        Event::Pointer(PointerEvent::Motion(motion)) => Some(InputEvent::PointerMotion {
            dx: motion.dx(),
            dy: motion.dy(),
        }),
        Event::Pointer(PointerEvent::MotionAbsolute(motion)) => {
            // libinput's `absolute_x/y_transformed(W)` maps the device's full
            // axis range to `0..W`.  Pass a large W and divide to recover a
            // normalised 0..1 coordinate; the backend scales to scanout size.
            const SCALE: u32 = 1_000_000;
            Some(InputEvent::PointerMotionAbsolute {
                x_norm: motion.absolute_x_transformed(SCALE) / SCALE as f64,
                y_norm: motion.absolute_y_transformed(SCALE) / SCALE as f64,
            })
        }
        Event::Pointer(PointerEvent::Button(btn)) => Some(InputEvent::Button {
            code: btn.button(),
            pressed: btn.button_state() == ButtonState::Pressed,
        }),
        Event::Pointer(PointerEvent::ScrollWheel(ev)) => {
            // Wheel events come pre-quantized in v120 (120 = one click).
            // has_axis(axis) MUST be checked first: libinput emits a
            // `client bug: value requested for unset axis` error if
            // scroll_value_v120 is called for an axis the event doesn't
            // carry. A pure vertical wheel event has Horizontal unset.
            let dx_v120 = if ev.has_axis(Axis::Horizontal) {
                ev.scroll_value_v120(Axis::Horizontal) as i32
            } else {
                0
            };
            let dy_v120 = if ev.has_axis(Axis::Vertical) {
                ev.scroll_value_v120(Axis::Vertical) as i32
            } else {
                0
            };
            if dx_v120 == 0 && dy_v120 == 0 {
                return None;
            }
            Some(InputEvent::PointerScroll { dx_v120, dy_v120 })
        }
        // ScrollFinger: a zero-delta event is libinput's fingers-lifted stop
        // (`finger_or_continuous_to_event` returns None only for all-zero
        // deltas), which we surface as PointerScrollStop. ScrollContinuous has
        // no finger-lift, so its zero deltas stay dropped.
        Event::Pointer(PointerEvent::ScrollFinger(ev)) => {
            Some(finger_or_continuous_to_event(ev).unwrap_or(InputEvent::PointerScrollStop))
        }
        Event::Pointer(PointerEvent::ScrollContinuous(ev)) => finger_or_continuous_to_event(ev),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::is_touchpad;

    /// Touchpad classification keys off libinput's tap finger count:
    /// mice / trackpoints / keyboards report 0; clickpads/touchpads
    /// report >= 1. (The config application itself is libinput FFI,
    /// verified on hardware — only the decision is unit-testable.)
    #[test]
    fn touchpad_classified_by_tap_finger_count() {
        assert!(!is_touchpad(0), "0 fingers = not a touchpad");
        assert!(is_touchpad(1), "1 finger = touchpad");
        assert!(is_touchpad(3), "3 fingers = touchpad");
    }
}
