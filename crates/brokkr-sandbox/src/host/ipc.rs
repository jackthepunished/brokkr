//! IPC plumbing between the host worker and the `brokkr-sandboxd` runner.
//!
//! Phase 2 keeps this small: a single anonymous pipe whose read end is
//! placed on file descriptor 3 in the runner, used to send a JSON-encoded
//! [`crate::SandboxConfig`]. The pipe is one-shot; the host writes the
//! payload and closes, the runner reads to EOF.

use std::io;
use std::os::fd::{IntoRawFd, OwnedFd, RawFd};

use nix::fcntl::OFlag;
use nix::unistd::pipe2;

/// A unidirectional config pipe.
///
/// Both ends are created with `O_CLOEXEC` so they automatically close on
/// `execve` in the runner. The host's `pre_exec` hook `dup2`s the read end
/// to file descriptor 3; `dup2` resets the close-on-exec flag on the
/// target, so fd 3 survives exec while the originals (and the inherited
/// copy of the write end in the child) close cleanly. Without `O_CLOEXEC`
/// the runner would inherit a copy of its own write end, `read_to_end`
/// would never see EOF, and the host would deadlock on `wait_with_output`.
pub(super) struct ConfigPipe {
    /// Write end (host-side).
    pub(super) writer: OwnedFd,
    /// Read end as a raw fd (intended for `dup2 → 3` in the child).
    pub(super) child_read_fd: RawFd,
}

pub(super) fn create_config_pipe() -> Result<ConfigPipe, io::Error> {
    let (read_end, write_end) = pipe2(OFlag::O_CLOEXEC).map_err(io::Error::from)?;
    Ok(ConfigPipe {
        writer: write_end,
        child_read_fd: read_end.into_raw_fd(),
    })
}
