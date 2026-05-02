use super::super::{Outcome, Status};

/// Check that the kernel is at least 5.10.
pub(super) fn check_kernel_version(release: Option<&str>) -> Outcome {
    const NAME: &str = "kernel ≥ 5.10";
    match release.and_then(parse_kernel_version) {
        Some((major, minor)) if (major, minor) >= (5, 10) => Outcome {
            name: NAME.to_string(),
            status: Status::Pass,
            detail: None,
        },
        Some((major, minor)) => Outcome {
            name: NAME.to_string(),
            status: Status::Fail,
            detail: Some(format!("found {major}.{minor}; sandbox requires 5.10+")),
        },
        None => Outcome {
            name: NAME.to_string(),
            status: Status::Fail,
            detail: Some("could not read /proc/sys/kernel/osrelease".to_string()),
        },
    }
}

/// Parse the leading `MAJOR.MINOR` from a kernel release string like
/// `6.6.87.2-microsoft-standard-WSL2`.
pub(super) fn parse_kernel_version(s: &str) -> Option<(u32, u32)> {
    let mut parts = s.split(['.', '-']);
    let major: u32 = parts.next()?.parse().ok()?;
    let minor: u32 = parts.next()?.parse().ok()?;
    Some((major, minor))
}
