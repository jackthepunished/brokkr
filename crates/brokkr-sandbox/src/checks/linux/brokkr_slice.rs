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

    // Retry loop: on AlreadyExists, clean up stale probe and retry once.
    let mut attempt = 0;
    loop {
        let probe = path.join(format!(
            "brokkr-check-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        match fs::create_dir(&probe) {
            Ok(()) => {
                // Probe created successfully. Remove it and report pass.
                if let Err(e) = fs::remove_dir(&probe) {
                    // Leak the probe dir on removal error but report the cleanup
                    // failure as a warning detail since the write succeeded.
                    return Outcome {
                        name: NAME.to_string(),
                        status: Status::Pass,
                        detail: Some(format!("probe {probe:?} created but failed to remove: {e}")),
                    };
                }
                return Outcome {
                    name: NAME.to_string(),
                    status: Status::Pass,
                    detail: None,
                };
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // Stale probe dir from a previous run — clean it up and retry.
                if attempt == 0 {
                    attempt += 1;
                    if fs::remove_dir_all(&probe).is_err() {
                        // Cannot clean up, fall through to failure below.
                    }
                    continue;
                }
                // Second attempt also AlreadyExists — something else is wrong.
                return Outcome {
                    name: NAME.to_string(),
                    status: Status::Fail,
                    detail: Some(format!("cannot mkdir under {SLICE} after retry: {e}")),
                };
            }
            Err(e) => {
                return Outcome {
                    name: NAME.to_string(),
                    status: Status::Fail,
                    detail: Some(format!("cannot mkdir under {SLICE}: {e}")),
                };
            }
        }
    }
}
