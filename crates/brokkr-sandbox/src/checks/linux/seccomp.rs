//! Seccomp check.

use std::fs;

use super::super::{Outcome, Status};

pub(crate) fn check_seccomp() -> Outcome {
    const NAME: &str = "seccomp-bpf available";
    // /proc/self/status lists `Seccomp:` on any kernel built with
    // CONFIG_SECCOMP. The numeric value (0/1/2) doesn't matter to us.
    match fs::read_to_string("/proc/self/status") {
        Ok(s) if s.lines().any(|l| l.starts_with("Seccomp:")) => Outcome {
            name: NAME.to_string(),
            status: Status::Pass,
            detail: None,
        },
        Ok(_) => Outcome {
            name: NAME.to_string(),
            status: Status::Fail,
            detail: Some(
                "/proc/self/status has no Seccomp: line — kernel built without CONFIG_SECCOMP"
                    .to_string(),
            ),
        },
        Err(e) => Outcome {
            name: NAME.to_string(),
            status: Status::Fail,
            detail: Some(format!("/proc/self/status: {e}")),
        },
    }
}
