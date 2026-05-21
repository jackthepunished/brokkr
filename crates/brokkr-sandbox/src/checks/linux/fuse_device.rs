//! `/dev/fuse` accessibility probe (Phase 3 M6b).
//!
//! The FUSE input mount in `brokkr-worker::fuse` needs read +
//! write access to `/dev/fuse`. We don't actually open it here —
//! that would race with whatever else might be using it — we just
//! check existence and the calling process's r/w access bits.
//!
//! Outcomes:
//! * `Pass`  — device exists and the worker uid has rw.
//! * `Warn`  — device exists but the worker can't rw it (e.g. WSL
//!   default where `/dev/fuse` is `0600 root:root`). The hint
//!   points at the usual remediation. Sandbox stays "functional"
//!   per [`super::super::Report::is_functional`]; an operator who
//!   never runs an action with a FUSE-mounted input tree can
//!   ignore the warning.
//! * `Fail`  — device missing entirely (no `fuse` kernel module).

use std::fs;
use std::path::Path;

use super::super::{Outcome, Status};

const DEV_FUSE: &str = "/dev/fuse";

/// Probe `/dev/fuse` for the M6b lazy-input mount.
pub(super) fn check_fuse_device() -> Outcome {
    const NAME: &str = "/dev/fuse accessible";
    let path = Path::new(DEV_FUSE);
    match fs::metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Outcome {
            name: NAME.to_string(),
            status: Status::Fail,
            detail: Some(
                "/dev/fuse does not exist — FUSE kernel module not loaded. \
                 On WSL2: `sudo modprobe fuse`. On a custom kernel: \
                 enable CONFIG_FUSE_FS."
                    .to_string(),
            ),
        },
        Err(e) => Outcome {
            name: NAME.to_string(),
            status: Status::Fail,
            detail: Some(format!("/dev/fuse: {e}")),
        },
        Ok(_) => match check_rw_access(path) {
            Ok(()) => Outcome {
                name: NAME.to_string(),
                status: Status::Pass,
                detail: None,
            },
            Err(detail) => Outcome {
                name: NAME.to_string(),
                status: Status::Warn,
                detail: Some(detail),
            },
        },
    }
}

// Returns `Ok(())` iff the calling process can open `/dev/fuse`
// for read+write. We use `access(2)` semantics rather than
// actually opening because opening might trip a kernel rate
// limit or interact badly with other FUSE consumers on the host.
fn check_rw_access(path: &Path) -> Result<(), String> {
    use nix::unistd::{access, AccessFlags};
    match access(path, AccessFlags::R_OK | AccessFlags::W_OK) {
        Ok(()) => Ok(()),
        Err(e) => Err(format!(
            "/dev/fuse exists but is not rw to the worker uid ({e}). \
             Try `sudo chmod 666 /dev/fuse` or add the worker user to \
             the `fuse` group (on distros that ship one)."
        )),
    }
}
