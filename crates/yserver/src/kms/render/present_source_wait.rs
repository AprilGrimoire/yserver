//! Asynchronous implicit-sync producer waits for `PresentPixmap` sources.

use std::os::fd::{AsFd, OwnedFd};

use nix::poll::{PollFd, PollFlags, PollTimeout, poll};

use crate::kms::render::store::DrawableId;

/// One imported source held alive until its producer sync-file becomes
/// readable and the core records the deferred copy.
pub(crate) struct PendingPresentSourceWait {
    pub(crate) fd: OwnedFd,
    pub(crate) source_id: DrawableId,
    /// False when registration in the stable completion poller failed. Such
    /// entries are still checked from the backend's 1 ms polling fallback.
    pub(crate) registered: bool,
    pub(crate) ready_reported: bool,
}

impl PendingPresentSourceWait {
    pub(crate) fn is_ready(&self) -> bool {
        let mut fds = [PollFd::new(self.fd.as_fd(), PollFlags::POLLIN)];
        match poll(&mut fds, PollTimeout::ZERO) {
            Ok(0) => false,
            Ok(_) => fds[0].revents().is_some_and(|flags| {
                flags.intersects(PollFlags::POLLIN | PollFlags::POLLERR | PollFlags::POLLHUP)
            }),
            Err(e) => {
                log::warn!("deferred Present source: poll(sync_file): {e}; treating as ready");
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::fd::OwnedFd;

    use nix::{
        sys::eventfd::{EfdFlags, EventFd},
        unistd::write,
    };

    use super::*;

    #[test]
    fn readiness_tracks_the_sync_file_signal() {
        let event =
            EventFd::from_value_and_flags(0, EfdFlags::EFD_NONBLOCK | EfdFlags::EFD_CLOEXEC)
                .expect("eventfd");
        let fd: OwnedFd = event.into();
        let wait = PendingPresentSourceWait {
            fd,
            source_id: DrawableId::for_tests(1),
            registered: false,
            ready_reported: false,
        };

        assert!(!wait.is_ready());
        write(&wait.fd, &1_u64.to_ne_bytes()).expect("signal eventfd");
        assert!(wait.is_ready());
    }
}
