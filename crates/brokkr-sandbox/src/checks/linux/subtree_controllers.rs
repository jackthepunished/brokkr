use super::super::{Outcome, Status};
use std::collections::HashSet;
use std::fs;

/// Check that the brokkr slice has all required controllers in subtree_control.
pub(super) fn check_subtree_controllers() -> Outcome {
    const NAME: &str = "brokkr slice cgroup controllers";
    const SLICE_SUBTREE_CONTROL: &str = "/sys/fs/cgroup/brokkr.slice/cgroup.subtree_control";
    const REQUIRED: &[&str] = &["cpu", "memory", "pids", "io"];

    let content = match fs::read_to_string(SLICE_SUBTREE_CONTROL) {
        Ok(c) => c,
        Err(e) => {
            return Outcome {
                name: NAME.to_string(),
                status: Status::Fail,
                detail: Some(format!("{SLICE_SUBTREE_CONTROL}: {e}")),
            };
        }
    };

    let controllers = parse_subtree_controllers(&content);

    let missing: Vec<&str> = REQUIRED
        .iter()
        .filter(|c| !controllers.contains(*c))
        .copied()
        .collect();

    if missing.is_empty() {
        Outcome {
            name: NAME.to_string(),
            status: Status::Pass,
            detail: None,
        }
    } else {
        Outcome {
            name: NAME.to_string(),
            status: Status::Fail,
            detail: Some(format!(
                "missing controllers in subtree_control: {} — re-run scripts/install-cgroup-slice.sh",
                missing.join(", ")
            )),
        }
    }
}

/// Parse a `cgroup.subtree_control` file contents into a set of controller
/// names. Handles `+`-prefixed controllers (e.g. `+cpu`) and trailing
/// whitespace.
pub(super) fn parse_subtree_controllers(content: &str) -> HashSet<&str> {
    content
        .split_whitespace()
        .map(|s| s.trim_start_matches('+'))
        .collect()
}
