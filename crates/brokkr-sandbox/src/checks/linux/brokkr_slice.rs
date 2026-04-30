//! Brokkr cgroup slice check.

use std::fs;
use std::path::Path;

use super::super::{Outcome, Status};

pub(crate) fn check_brokkr_slice() -> Outcome {
    const NAME: &str = "brokkr cgroup slice writable";
    const SLICE: &str = "/sys/fs/cgroup/brokkr.slice";
    let path = Path::new(SLICE);
    if !path.exists() {
        return Outcome {
            name: NAME.to_string(),
            status: Status::Fail,
            detail: Some(format!(
                "{SLICE} missing — run scripts/install-cgroup-slice.sh"
            )),
        };
    }
    // A real write probe: try to mkdir under the slice and immediately
    // remove it. Cgroup files don't honour open(O_RDWR) consistently, so
    // mkdir is the only reliable test.
    let probe = path.join(format!("brokkr-check-{}", std::process::id()));
    match fs::create_dir(&probe) {
        Ok(()) => {
            let _ = fs::remove_dir(&probe);
            Outcome {
                name: NAME.to_string(),
                status: Status::Pass,
                detail: None,
            }
        }
        Err(e) => Outcome {
            name: NAME.to_string(),
            status: Status::Fail,
            detail: Some(format!("cannot mkdir under {SLICE}: {e}")),
        },
    }
}
