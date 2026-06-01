//! Env-gated (`YSERVER_GRAB_DEBUG=1`) single-stream diagnostic timeline
//! for the cinnamon keyring/polkit "can't dismiss" investigation.
//!
//! Background (see memory `project_cinnamon_keyring_grab`): the keyring
//! prompt is a Clutter modal drawn inside the cinnamon shell's
//! full-screen stage. Cinnamon captures modal input via an active
//! `XIGrabDevice` on the stage (`owner_events=false`) and *re-grabs
//! around every click*. Under yserver that re-grab loop dies after a
//! few clicks, so the next click falls through the stage's input shape
//! to nemo-desktop and the dialog can no longer be dismissed.
//!
//! The xtrace already shows what cinnamon sends/receives on the wire.
//! What it can't show is yserver's *internal* grab/focus bookkeeping and
//! the per-click hit-test + routing decision. This module prints one
//! greppable `[grab-dbg]` line at each decision boundary so the
//! server-side timeline can be lined up against the xtrace without
//! enabling firehose `RUST_LOG=…=debug`.
//!
//! Stripped once the keyring grab bug is root-caused.

use std::sync::OnceLock;

use crate::server::ServerState;

static ENABLED: OnceLock<bool> = OnceLock::new();

/// True when `YSERVER_GRAB_DEBUG` is set in the environment (cached).
#[must_use]
pub fn enabled() -> bool {
    *ENABLED.get_or_init(|| std::env::var_os("YSERVER_GRAB_DEBUG").is_some())
}

/// One-line snapshot of every pointer/keyboard grab + the keyboard
/// focus window. Cheap enough to build only behind `enabled()`.
#[must_use]
pub fn snapshot(state: &ServerState) -> String {
    let ptr_grab = match state.pointer_grab {
        Some((c, w)) => format!(
            "ptr_grab{{owner={} win=0x{:x} passive={}}}",
            c.0, w.0, state.pointer_grab_is_passive
        ),
        None => "ptr_grab=none".to_string(),
    };
    let ptr_active = match state.active_pointer_grab {
        Some(g) => format!(
            "ptr_active{{owner={} win=0x{:x} owner_events={}}}",
            g.owner.0, g.grab_window.0, g.owner_events
        ),
        None => "ptr_active=none".to_string(),
    };
    let kbd_active = match state.active_keyboard_grab {
        Some(g) => format!(
            "kbd_active{{owner={} win=0x{:x} src={:?}}}",
            g.owner.0, g.grab_window.0, g.source
        ),
        None => "kbd_active=none".to_string(),
    };
    let focus = crate::core_loop::key_fanout::current_focus(state);
    format!("focus=0x{:x} {ptr_grab} {ptr_active} {kbd_active}", focus.0)
}

/// Print `[grab-dbg] <msg> | <state snapshot>` to stderr when gated on.
pub fn log(state: &ServerState, msg: &str) {
    if !enabled() {
        return;
    }
    eprintln!("[grab-dbg] {msg} | {}", snapshot(state));
}
