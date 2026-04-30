//! Run all host-compatibility probes and return a structured report.

use crate::checks::{Outcome, Report, Status};

/// Run all host-compatibility probes and return a structured report.
///
/// On non-Linux hosts the report contains a single `Fail` outcome since the
/// Phase 2 sandbox is Linux-only.
pub fn run() -> Report {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;

    #[cfg(target_os = "linux")]
    {
        crate::checks::linux::run(os, arch)
    }

    #[cfg(not(target_os = "linux"))]
    {
        Report {
            os,
            arch,
            kernel_release: None,
            outcomes: vec![Outcome {
                name: "linux required".to_string(),
                status: Status::Fail,
                detail: Some(format!(
                    "the Phase 2 sandbox is Linux-only; this host is {os}"
                )),
            }],
        }
    }
}
