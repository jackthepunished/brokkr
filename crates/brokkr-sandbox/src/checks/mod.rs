//! Linux host-compatibility probes for the Phase 2 sandbox.
//!
//! Returns a structured [`Report`] rather than printing directly so different
//! callers (the worker's `--check-host` flag, a future `brokk doctor`, and
//! integration tests) can format it as they wish. Linux-only checks are
//! gated on `target_os = "linux"`; on other hosts the report contains a
//! single `Fail` outcome explaining why.
//!
//! See `docs/phase-2-plan.md` §10.3 for the canonical output format.

use std::fmt;

pub mod linux;

/// Pass / warn / fail outcome of a single probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Passes; no concern.
    Pass,
    /// Functional but degraded — a fallback path will be used.
    Warn,
    /// Sandbox cannot start without this fixed.
    Fail,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Status::Pass => " OK  ",
            Status::Warn => "WARN ",
            Status::Fail => "FAIL ",
        }
    }
}

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

/// Run all host-compatibility probes and return a structured report.
///
/// On non-Linux hosts the report contains a single `Fail` outcome since the
/// Phase 2 sandbox is Linux-only.
pub fn run() -> Report {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;

    #[cfg(target_os = "linux")]
    {
        let kernel_release = std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .ok()
            .map(|s| s.trim().to_string());
        let outcomes = linux::run_linux(kernel_release.as_ref().map(|s| s.as_str()));
        Report {
            os,
            arch,
            kernel_release,
            outcomes,
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_labels_match_plan_format() {
        assert_eq!(Status::Pass.label(), " OK  ");
        assert_eq!(Status::Warn.label(), "WARN ");
        assert_eq!(Status::Fail.label(), "FAIL ");
    }

    #[test]
    fn counts_and_functional() {
        let r = Report {
            os: "linux",
            arch: "x86_64",
            kernel_release: Some("6.6.0".to_string()),
            outcomes: vec![
                Outcome {
                    name: "a".into(),
                    status: Status::Pass,
                    detail: None,
                },
                Outcome {
                    name: "b".into(),
                    status: Status::Warn,
                    detail: Some("note".into()),
                },
                Outcome {
                    name: "c".into(),
                    status: Status::Pass,
                    detail: None,
                },
            ],
        };
        assert_eq!(r.counts(), (0, 1));
        assert!(r.is_functional());

        let mut bad = r.clone();
        bad.outcomes.push(Outcome {
            name: "d".into(),
            status: Status::Fail,
            detail: None,
        });
        assert_eq!(bad.counts(), (1, 1));
        assert!(!bad.is_functional());
    }

    #[test]
    fn display_summary_branches() {
        let r = Report {
            os: "linux",
            arch: "x86_64",
            kernel_release: Some("6.6.0".into()),
            outcomes: vec![Outcome {
                name: "ok".into(),
                status: Status::Pass,
                detail: None,
            }],
        };
        let out = format!("{r}");
        assert!(out.contains("Sandbox is functional."));
        assert!(!out.contains("warning"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_kernel_version_basic() {
        use linux::kernel_version::parse_kernel_version;
        assert_eq!(parse_kernel_version("5.10.0"), Some((5, 10)));
        assert_eq!(parse_kernel_version("6.6.87.2-microsoft"), Some((6, 6)));
        assert_eq!(parse_kernel_version("6.10-rc2"), Some((6, 10)));
        assert_eq!(parse_kernel_version("not.a.version"), None);
        assert_eq!(parse_kernel_version(""), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_subtree_controllers_basic() {
        use linux::subtree_controllers::parse_subtree_controllers;

        // All four required controllers present
        let set = parse_subtree_controllers("cpu memory pids io");
        assert!(set.contains("cpu"));
        assert!(set.contains("memory"));
        assert!(set.contains("pids"));
        assert!(set.contains("io"));
        assert_eq!(set.len(), 4);

        // Plus-prefixed controllers (as written by cgroup filesystem)
        let set = parse_subtree_controllers("+cpu +memory +pids +io");
        assert!(set.contains("cpu"));
        assert!(set.contains("memory"));
        assert!(set.contains("pids"));
        assert!(set.contains("io"));

        // Mixed prefixes
        let set = parse_subtree_controllers("+cpu memory +pids io");
        assert!(set.contains("cpu"));
        assert!(set.contains("memory"));

        // Trailing whitespace
        let set = parse_subtree_controllers("cpu memory pids io   \n");
        assert_eq!(set.len(), 4);

        // Extra controllers not in REQUIRED list — parser retains them (rdma,
        // hugetlb appear in the set). The checker later filters against REQUIRED
        // when determining which controllers are missing.
        let set = parse_subtree_controllers("cpu memory pids io rdma hugetlb");
        assert_eq!(set.len(), 6);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_run_produces_eight_outcomes() {
        let report = run();
        assert_eq!(report.os, "linux");
        assert_eq!(report.outcomes.len(), 8);
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn non_linux_run_reports_unsupported() {
        let report = run();
        assert!(!report.is_functional());
        assert_eq!(report.outcomes.len(), 1);
    }
}
