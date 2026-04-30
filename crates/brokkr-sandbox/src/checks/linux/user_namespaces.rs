//! User namespaces check.

use std::fs;

use super::super::{Outcome, Status};

pub(crate) fn check_user_namespaces() -> Outcome {
    const NAME: &str = "unprivileged user namespaces enabled";
    // /proc/sys/user/max_user_namespaces is universal across distros.
    // /proc/sys/kernel/unprivileged_userns_clone is a Debian/Ubuntu-specific
    // gate (= 0 disables the unprivileged path even when the kernel supports
    // userns). Both must be permissive.
    let max = fs::read_to_string("/proc/sys/user/max_user_namespaces")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok());
    let gate = fs::read_to_string("/proc/sys/kernel/unprivileged_userns_clone")
        .ok()
        .map(|s| s.trim().to_string());
    match (max, gate.as_deref()) {
        (Some(0), _) => Outcome {
            name: NAME.to_string(),
            status: Status::Fail,
            detail: Some("user.max_user_namespaces = 0".to_string()),
        },
        (Some(_), Some("1") | None) => Outcome {
            name: NAME.to_string(),
            status: Status::Pass,
            detail: None,
        },
        (Some(_), Some(other)) => Outcome {
            name: NAME.to_string(),
            status: Status::Fail,
            detail: Some(format!(
                "kernel.unprivileged_userns_clone = {other} (the Debian/Ubuntu gate)"
            )),
        },
        (None, _) => Outcome {
            name: NAME.to_string(),
            status: Status::Fail,
            detail: Some("could not read /proc/sys/user/max_user_namespaces".to_string()),
        },
    }
}
