//! Swapchain — produce/scanout state machine.
//!
//! Pure logic in [`SwapState`]; the buffer-owning [`Swapchain`] composes a
//! `SwapState` with a `Vec<DumbBuffer>` and delegates state transitions.

use crate::drm::DumbBuffer;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BufferState {
    Free,
    Acquired,
    Submitted,
    Scanout,
}

#[derive(Debug)]
pub struct SwapState {
    states: Vec<BufferState>,
}

impl SwapState {
    pub fn new(n: usize) -> Self {
        Self {
            states: vec![BufferState::Free; n],
        }
    }

    pub fn with_initial_scanout(n: usize, idx: usize) -> Self {
        assert!(idx < n, "initial scanout index out of range");
        let mut states = vec![BufferState::Free; n];
        states[idx] = BufferState::Scanout;
        Self { states }
    }

    pub fn count_free(&self) -> usize {
        self.states
            .iter()
            .filter(|s| **s == BufferState::Free)
            .count()
    }

    pub fn count_scanout(&self) -> usize {
        self.states
            .iter()
            .filter(|s| **s == BufferState::Scanout)
            .count()
    }

    /// Index of the buffer currently in `Submitted` state, if any.
    /// Used by the page-flip completion path: the kernel event doesn't
    /// preserve our user_data through the drm crate's parser, so we
    /// rely on the invariant that at most one buffer is Submitted.
    pub fn submitted_idx(&self) -> Option<usize> {
        self.states
            .iter()
            .position(|s| *s == BufferState::Submitted)
    }

    pub fn acquire(&mut self) -> Option<usize> {
        let idx = self.states.iter().position(|s| *s == BufferState::Free)?;
        self.states[idx] = BufferState::Acquired;
        Some(idx)
    }

    pub fn submit(&mut self, idx: usize) -> Result<(), &'static str> {
        if idx >= self.states.len() {
            return Err("buffer index out of range");
        }
        if self.states[idx] != BufferState::Acquired {
            return Err("submit called on non-Acquired buffer");
        }
        self.states[idx] = BufferState::Submitted;
        Ok(())
    }

    pub fn complete(&mut self, idx: usize) -> Result<(), &'static str> {
        if idx >= self.states.len() {
            return Err("buffer index out of range");
        }
        if self.states[idx] != BufferState::Submitted {
            return Err("complete called on non-Submitted buffer");
        }
        for state in &mut self.states {
            if *state == BufferState::Scanout {
                *state = BufferState::Free;
            }
        }
        self.states[idx] = BufferState::Scanout;
        Ok(())
    }

    /// Release a previously-acquired buffer back to `Free` without
    /// going through submit/complete. Used by the PixmanShadow scanout
    /// path in [`crate::kms::backend::KmsBackend`]: pixman paints
    /// into the dumb buffer as a transient destination, the result is
    /// uploaded into a `ScanoutBo` `VkImage`, and the dumb buffer is
    /// not actually flipped — so it should return to the free pool
    /// immediately, not stay parked in `Acquired`.
    pub fn release_acquired(&mut self, idx: usize) -> Result<(), &'static str> {
        if idx >= self.states.len() {
            return Err("buffer index out of range");
        }
        if self.states[idx] != BufferState::Acquired {
            return Err("release_acquired called on non-Acquired buffer");
        }
        self.states[idx] = BufferState::Free;
        Ok(())
    }
}

pub struct Swapchain {
    buffers: Vec<DumbBuffer>,
    state: SwapState,
}

impl Swapchain {
    /// Disarm every buffer in this swapchain so its `Drop` becomes
    /// a no-op. **Only valid at final process exit.** Called from
    /// the shutdown path when atomic `disable_output` failed for
    /// this output — see `DumbBuffer::disarm` doc.
    pub fn disarm(&mut self) {
        for buf in &mut self.buffers {
            buf.disarm();
        }
    }

    /// Construct an empty `Swapchain` for tests — no buffers, no
    /// pending scanout. Hidden from rustdoc; for fixture use only.
    #[doc(hidden)]
    #[must_use]
    pub fn empty_for_tests() -> Self {
        Self {
            buffers: Vec::new(),
            state: SwapState::new(0),
        }
    }

    pub fn with_initial_scanout(buffers: Vec<DumbBuffer>, scanout_idx: usize) -> Self {
        let n = buffers.len();
        Self {
            buffers,
            state: SwapState::with_initial_scanout(n, scanout_idx),
        }
    }

    pub fn buffer(&self, idx: usize) -> &DumbBuffer {
        &self.buffers[idx]
    }

    pub fn buffer_mut(&mut self, idx: usize) -> &mut DumbBuffer {
        &mut self.buffers[idx]
    }

    pub fn acquire(&mut self) -> Option<&mut DumbBuffer> {
        let idx = self.state.acquire()?;
        Some(&mut self.buffers[idx])
    }

    pub fn acquire_idx(&mut self) -> Option<usize> {
        self.state.acquire()
    }

    pub fn submit(&mut self, idx: usize) -> Result<(), &'static str> {
        self.state.submit(idx)
    }

    pub fn complete(&mut self, idx: usize) -> Result<(), &'static str> {
        self.state.complete(idx)
    }

    pub fn release_acquired(&mut self, idx: usize) -> Result<(), &'static str> {
        self.state.release_acquired(idx)
    }

    pub fn submitted_idx(&self) -> Option<usize> {
        self.state.submitted_idx()
    }

    pub fn len(&self) -> usize {
        self.buffers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buffers.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_swapchain_has_all_free() {
        let s = SwapState::new(3);
        assert_eq!(s.count_free(), 3);
    }

    #[test]
    fn with_initial_scanout_marks_buffer_busy() {
        let s = SwapState::with_initial_scanout(3, 0);
        assert_eq!(s.count_free(), 2);
        assert_eq!(s.count_scanout(), 1);
    }

    #[test]
    fn acquire_never_returns_initial_scanout_buffer() {
        let mut s = SwapState::with_initial_scanout(2, 0);
        let i = s.acquire().unwrap();
        assert_ne!(i, 0);
    }

    #[test]
    fn acquire_then_submit_then_complete_advances_state() {
        let mut s = SwapState::new(3);
        let i = s.acquire().unwrap();
        s.submit(i).unwrap();
        s.complete(i).unwrap();
        assert_eq!(s.count_free(), 2);
        assert_eq!(s.count_scanout(), 1);
    }

    #[test]
    fn second_complete_releases_first() {
        let mut s = SwapState::new(3);
        let a = s.acquire().unwrap();
        s.submit(a).unwrap();
        s.complete(a).unwrap();
        let b = s.acquire().unwrap();
        s.submit(b).unwrap();
        s.complete(b).unwrap();
        assert_eq!(s.count_free(), 2);
        assert_eq!(s.count_scanout(), 1);
    }

    #[test]
    fn acquire_returns_none_when_all_busy() {
        let mut s = SwapState::new(2);
        let _a = s.acquire().unwrap();
        let _b = s.acquire().unwrap();
        assert!(s.acquire().is_none());
    }

    #[test]
    fn submit_unacquired_buffer_errors() {
        let mut s = SwapState::new(2);
        assert!(s.submit(0).is_err());
    }

    #[test]
    fn complete_unsubmitted_buffer_errors() {
        let mut s = SwapState::new(2);
        let i = s.acquire().unwrap();
        assert!(s.complete(i).is_err());
    }

    #[test]
    fn submitted_idx_tracks_at_most_one_buffer() {
        let mut s = SwapState::new(3);
        assert_eq!(s.submitted_idx(), None);
        let i = s.acquire().unwrap();
        s.submit(i).unwrap();
        assert_eq!(s.submitted_idx(), Some(i));
        s.complete(i).unwrap();
        assert_eq!(s.submitted_idx(), None);
    }
}
