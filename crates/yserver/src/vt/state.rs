//! Pure VT-switch session state machine. No I/O — drives the suspend/resume
//! coordination from VT_PROCESS release/acquire (delivered as SIGUSR1/SIGUSR2
//! and routed via `Message::VtRelease`/`VtAcquire` → `drive_vt_event`).
//!
//! Spec: docs/superpowers/specs/2026-05-27-vt-switching-design.md §"State machine".

#![allow(dead_code)]

/// Server-wide seat session state. `Suspending`/`Resuming` are transient
/// states bracketing the (possibly long) sequences.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VtState {
    Active,
    Suspending,
    Suspended,
    Resuming,
}

/// Coalesced counter-events. We never queue more than one of each.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VtPending {
    pub pending_enable: bool,
    pub pending_disable: bool,
}

/// A VT-switch event (VT_PROCESS release/acquire, delivered as SIGUSR1/2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VtEventKind {
    Enable,
    Disable,
}

/// What the caller must do after applying an event to the state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VtAction {
    /// Run the suspend sequence (then call [`VtState::suspend_complete`]).
    BeginSuspend,
    /// Run the resume sequence (then call [`VtState::resume_complete`]).
    BeginResume,
    /// Do nothing this turn.
    Nothing,
}

impl VtState {
    /// True only when master-requiring I/O (modeset, pageflip, submit)
    /// is allowed. Gate every such operation on this.
    #[must_use]
    pub fn allows_scanout(self) -> bool {
        matches!(self, VtState::Active)
    }

    /// Apply a VT-switch event. Mutates `pending`, returns the action the
    /// caller must perform. Mirrors the spec's event×state matrix.
    pub fn on_event(&mut self, pending: &mut VtPending, ev: VtEventKind) -> VtAction {
        match (*self, ev) {
            (VtState::Active, VtEventKind::Disable) => {
                *self = VtState::Suspending;
                VtAction::BeginSuspend
            }
            (VtState::Suspended, VtEventKind::Enable) => {
                *self = VtState::Resuming;
                VtAction::BeginResume
            }
            // Coalesce a counter-event that arrives mid-sequence.
            (VtState::Suspending, VtEventKind::Enable)
            | (VtState::Resuming, VtEventKind::Enable) => {
                pending.pending_enable = true;
                VtAction::Nothing
            }
            (VtState::Resuming, VtEventKind::Disable) => {
                pending.pending_disable = true;
                VtAction::Nothing
            }
            // Everything else is a no-op (log warn at the call site):
            // Active+Enable, Suspended+Disable, Suspending+Disable.
            _ => VtAction::Nothing,
        }
    }

    /// Call after the suspend sequence finishes (VT_PROCESS release ack sent).
    /// Commits to `Suspended`. If an enable arrived meanwhile, the
    /// pending flag is left set so the next real `Enable` acts at once.
    pub fn suspend_complete(&mut self, _pending: &VtPending) {
        debug_assert_eq!(*self, VtState::Suspending);
        *self = VtState::Suspended;
    }

    /// Call after the resume sequence finishes but BEFORE committing to
    /// `Active`. If a disable arrived during resume, go straight back
    /// into `Suspending` (returning `BeginSuspend`) without ever
    /// becoming `Active` — avoids a visible "blink". Otherwise commit
    /// to `Active`.
    pub fn resume_complete(&mut self, pending: &mut VtPending) -> VtAction {
        debug_assert_eq!(*self, VtState::Resuming);
        if pending.pending_disable {
            pending.pending_disable = false;
            *self = VtState::Suspending;
            VtAction::BeginSuspend
        } else {
            *self = VtState::Active;
            VtAction::Nothing
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p() -> VtPending {
        VtPending::default()
    }

    #[test]
    fn active_disable_begins_suspend() {
        let mut s = VtState::Active;
        let mut pend = p();
        assert_eq!(
            s.on_event(&mut pend, VtEventKind::Disable),
            VtAction::BeginSuspend
        );
        assert_eq!(s, VtState::Suspending);
    }

    #[test]
    fn suspended_enable_begins_resume() {
        let mut s = VtState::Suspended;
        let mut pend = p();
        assert_eq!(
            s.on_event(&mut pend, VtEventKind::Enable),
            VtAction::BeginResume
        );
        assert_eq!(s, VtState::Resuming);
    }

    #[test]
    fn enable_during_suspend_is_coalesced() {
        let mut s = VtState::Suspending;
        let mut pend = p();
        assert_eq!(
            s.on_event(&mut pend, VtEventKind::Enable),
            VtAction::Nothing
        );
        assert!(pend.pending_enable);
        assert_eq!(s, VtState::Suspending);
    }

    #[test]
    fn disable_during_resume_is_coalesced() {
        let mut s = VtState::Resuming;
        let mut pend = p();
        assert_eq!(
            s.on_event(&mut pend, VtEventKind::Disable),
            VtAction::Nothing
        );
        assert!(pend.pending_disable);
    }

    #[test]
    fn double_disable_is_ignored() {
        let mut s = VtState::Suspending;
        let mut pend = p();
        assert_eq!(
            s.on_event(&mut pend, VtEventKind::Disable),
            VtAction::Nothing
        );
        assert_eq!(s, VtState::Suspending);
    }

    #[test]
    fn active_enable_is_ignored() {
        let mut s = VtState::Active;
        let mut pend = p();
        assert_eq!(
            s.on_event(&mut pend, VtEventKind::Enable),
            VtAction::Nothing
        );
        assert_eq!(s, VtState::Active);
    }

    #[test]
    fn resume_completion_bypasses_active_when_disable_pending() {
        let mut s = VtState::Resuming;
        let mut pend = VtPending {
            pending_disable: true,
            ..p()
        };
        assert_eq!(s.resume_complete(&mut pend), VtAction::BeginSuspend);
        assert_eq!(s, VtState::Suspending);
        assert!(!pend.pending_disable, "pending_disable consumed");
    }

    #[test]
    fn resume_completion_commits_active_when_nothing_pending() {
        let mut s = VtState::Resuming;
        let mut pend = p();
        assert_eq!(s.resume_complete(&mut pend), VtAction::Nothing);
        assert_eq!(s, VtState::Active);
    }

    #[test]
    fn only_active_allows_scanout() {
        assert!(VtState::Active.allows_scanout());
        assert!(!VtState::Suspending.allows_scanout());
        assert!(!VtState::Suspended.allows_scanout());
        assert!(!VtState::Resuming.allows_scanout());
    }
}
