//! VT-switch state machine for the Direct seat model.
//!
//! yserver is always Direct (self-managed DRM master + VT_PROCESS); there is
//! no libseat/logind session management. VT switching is driven by the console
//! guard: SIGUSR1/SIGUSR2 → `Message::VtRelease`/`VtAcquire` → `drive_vt_event`
//! → this state machine, which gates scanout across suspend/resume.

pub mod state;
