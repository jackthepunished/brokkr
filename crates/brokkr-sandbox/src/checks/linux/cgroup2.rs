use super::super::{Outcome, Status};
use std::fs;

/// Check that cgroup v2 unified hierarchy is mounted.
pub(super) fn check_cgroup_v2() -> Outcome {
    const NAME: &str = "cgroup v2 unified hierarchy";
    let mounts = match fs::read_to_string("/proc/mounts") {
        Ok(s) => s,
        Err(e) => {
            return Outcome {
                name: NAME.to_string(),
                status: Status::Fail,
                detail: Some(format!("/proc/mounts: {e}")),
            };
        }
    };
    // /proc/mounts lines: "<source> <mountpoint> <fstype> <options>..."
    let unified = mounts.lines().any(|l| {
        let mut it = l.split_ascii_whitespace();
        let _source = it.next();
        let mountpoint = it.next();
        let fstype = it.next();
        mountpoint == Some("/sys/fs/cgroup") && fstype == Some("cgroup2")
    });
    if unified {
        Outcome {
            name: NAME.to_string(),
            status: Status::Pass,
            detail: None,
        }
    } else {
        Outcome {
            name: NAME.to_string(),
            status: Status::Fail,
            detail: Some(
                "/sys/fs/cgroup is not cgroup2 — host is on cgroup v1 or hybrid".to_string(),
            ),
        }
    }
}
