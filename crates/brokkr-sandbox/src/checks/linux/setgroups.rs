//! setgroups check.

use std::path::Path;

use super::super::{Outcome, Status};

pub(crate) fn check_setgroups() -> Outcome {
    const NAME: &str = "/proc/self/setgroups present";
    if Path::new("/proc/self/setgroups").exists() {
        Outcome {
            name: NAME.to_string(),
            status: Status::Pass,
            detail: None,
        }
    } else {
        Outcome {
            name: NAME.to_string(),
            status: Status::Fail,
            detail: Some("required for gid_map writes under unprivileged userns".to_string()),
        }
    }
}
