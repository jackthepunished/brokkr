//! Memory peak check.

use std::path::Path;

use super::super::{Outcome, Status};

pub(crate) fn check_memory_peak(release: Option<&str>) -> Outcome {
    const NAME: &str = "cgroup memory.peak (kernel ≥ 5.19)";
    if Path::new("/sys/fs/cgroup/memory.peak").exists() {
        return Outcome {
            name: NAME.to_string(),
            status: Status::Pass,
            detail: None,
        };
    }
    let kver = release.unwrap_or("unknown");
    Outcome {
        name: NAME.to_string(),
        status: Status::Warn,
        detail: Some(format!(
            "absent on kernel {kver}; falling back to memory.events on the per-action cgroup"
        )),
    }
}
