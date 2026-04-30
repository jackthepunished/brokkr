//! Outcome types for host-compatibility checks.

use std::fmt;

use super::Status;

/// Result of a single named check.
#[derive(Debug, Clone)]
pub struct Outcome {
    /// Short, human-readable name.
    pub name: String,
    /// Pass / warn / fail.
    pub status: Status,
    /// Optional remediation hint or detail (e.g. the value we read).
    pub detail: Option<String>,
}

/// All check outcomes for one host probe pass.
#[derive(Debug, Clone)]
pub struct Report {
    /// OS string (`linux`, `macos`, …).
    pub os: &'static str,
    /// CPU arch (`x86_64`, `aarch64`, …).
    pub arch: &'static str,
    /// Kernel release string when readable from `/proc/sys/kernel/osrelease`.
    pub kernel_release: Option<String>,
    /// Outcome list, in deterministic order.
    pub outcomes: Vec<Outcome>,
}

impl Report {
    /// True iff every outcome is `Pass` or `Warn` (no `Fail`).
    pub fn is_functional(&self) -> bool {
        !self
            .outcomes
            .iter()
            .any(|o| matches!(o.status, Status::Fail))
    }

    /// `(failures, warnings)` count.
    pub fn counts(&self) -> (usize, usize) {
        let mut failures = 0;
        let mut warnings = 0;
        for o in &self.outcomes {
            match o.status {
                Status::Fail => failures += 1,
                Status::Warn => warnings += 1,
                Status::Pass => {}
            }
        }
        (failures, warnings)
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kernel = self.kernel_release.as_deref().unwrap_or("unknown");
        writeln!(
            f,
            "brokkr host compatibility check ({} {}, kernel {})",
            self.os, self.arch, kernel
        )?;
        writeln!(f)?;
        for o in &self.outcomes {
            match &o.detail {
                Some(d) => writeln!(f, "[{}] {} — {}", o.status.label(), o.name, d)?,
                None => writeln!(f, "[{}] {}", o.status.label(), o.name)?,
            }
        }
        let (fail, warn) = self.counts();
        writeln!(f)?;
        if fail > 0 {
            writeln!(
                f,
                "Sandbox is NOT functional. {fail} failure{}, {warn} warning{}.",
                plural(fail),
                plural(warn),
            )
        } else if warn > 0 {
            writeln!(f, "Sandbox is functional. {warn} warning{}.", plural(warn))
        } else {
            writeln!(f, "Sandbox is functional.")
        }
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}
