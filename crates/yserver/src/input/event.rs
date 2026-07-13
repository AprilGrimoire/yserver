//! yserver-local input event enum.
//!
//! Deliberately minimal: keycodes, pointer deltas, button + state.
//! No keysym translation — that's xkbcommon's job and lives in C.

use yserver_core::core_loop::DeviceInfo;

#[derive(Debug, Clone)]
pub enum InputEvent {
    KeyPress {
        keycode: u32,
    },
    KeyRelease {
        keycode: u32,
    },
    /// Relative pointer motion (mouse).
    PointerMotion {
        dx: f64,
        dy: f64,
    },
    /// Absolute pointer motion (tablet).  Coordinates are in 0..1 over the
    /// device's logical surface; the backend scales to scanout dimensions.
    PointerMotionAbsolute {
        x_norm: f64,
        y_norm: f64,
    },
    Button {
        code: u32,
        pressed: bool,
    },
    /// Pointer scroll wheel / two-finger / continuous scroll, in v120
    /// high-resolution units. 120 v120 ≈ one "click" of a discrete wheel.
    /// `dx_v120 > 0` is scroll-right, `dy_v120 > 0` is scroll-down (matches
    /// libinput's convention).
    PointerScroll {
        dx_v120: i32,
        dy_v120: i32,
    },
    /// Two-finger scroll ended (fingers lifted). libinput emits a
    /// finger-scroll event with all axes 0 to mark the stop; we forward it
    /// so the backend can emit an XI2 delta-0 scroll motion, which GDK turns
    /// into `scroll.is_stop = TRUE`. Firefox's SwipeTracker uses that stop to
    /// commit a horizontal-swipe history navigation (bug 1539730); without it
    /// the swipe arrow appears but never fires. Only `ScrollFinger` produces
    /// this — `ScrollContinuous`/`ScrollWheel` have no finger-lift.
    PointerScrollStop,
    /// A new input device has been enumerated by libinput.  Carries a
    /// snapshot of its identity and configuration; forwarded to the core for
    /// Task 2's XI2 device-property registry.
    DeviceAdded(DeviceInfo),
    /// An input device has been removed; matched by the evdev device node
    /// that was reported at add time.
    DeviceRemoved {
        device_node: String,
    },
}
