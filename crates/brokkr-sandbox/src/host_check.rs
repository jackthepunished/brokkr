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
        linux::run(os, arch)
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

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::fs;
    use std::path::Path;

    pub fn run(os: &'static str, arch: &'static str) -> Report {
        let kernel_release = fs::read_to_string("/proc/sys/kernel/osrelease")
            .ok()
            .map(|s| s.trim().to_string());

        let outcomes = vec![
            check_kernel_version(kernel_release.as_deref()),
            check_user_namespaces(),
            check_cgroup_v2(),
            check_brokkr_slice(),
            check_seccomp(),
            check_memory_peak(kernel_release.as_deref()),
            check_setgroups(),
        ];

        Report {
            os,
            arch,
            kernel_release,
            outcomes,
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

    fn check_kernel_version(release: Option<&str>) -> Outcome {
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

    fn check_user_namespaces() -> Outcome {
        const NAME: &str = "unprivileged user namespaces enabled";
        // /proc/sys/user/max_user_namespaces is universal across distros.
        // /proc/sys/kernel/unprivileged_userns_clone is a Debian/Ubuntu-specific
        // gate (= 0 disables the unprivileged path even when the kernel supports
        // userns). Both must be permissive.
        let max = fs::read_to_string("/proc/sys/user/max_user_namespaces")
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok());
        let gate = fs::read_to_string("/proc/sys/kernel/unprivileged_userns_clone")
            .ok()
            .map(|s| s.trim().to_string());
        match (max, gate.as_deref()) {
            (Some(0), _) => Outcome {
                name: NAME.to_string(),
                status: Status::Fail,
                detail: Some("user.max_user_namespaces = 0".to_string()),
            },
            (Some(_), Some("1") | None) => Outcome {
                name: NAME.to_string(),
                status: Status::Pass,
                detail: None,
            },
            (Some(_), Some(other)) => Outcome {
                name: NAME.to_string(),
                status: Status::Fail,
                detail: Some(format!(
                    "kernel.unprivileged_userns_clone = {other} (the Debian/Ubuntu gate)"
                )),
            },
            (None, _) => Outcome {
                name: NAME.to_string(),
                status: Status::Fail,
                detail: Some("could not read /proc/sys/user/max_user_namespaces".to_string()),
            },
        }
    }

    fn check_cgroup_v2() -> Outcome {
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

    fn check_brokkr_slice() -> Outcome {
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

    fn check_seccomp() -> Outcome {
        const NAME: &str = "seccomp-bpf available";
        // /proc/self/status lists `Seccomp:` on any kernel built with
        // CONFIG_SECCOMP. The numeric value (0/1/2) doesn't matter to us.
        match fs::read_to_string("/proc/self/status") {
            Ok(s) if s.lines().any(|l| l.starts_with("Seccomp:")) => Outcome {
                name: NAME.to_string(),
                status: Status::Pass,
                detail: None,
            },
            Ok(_) => Outcome {
                name: NAME.to_string(),
                status: Status::Fail,
                detail: Some(
                    "/proc/self/status has no Seccomp: line — kernel built without CONFIG_SECCOMP"
                        .to_string(),
                ),
            },
            Err(e) => Outcome {
                name: NAME.to_string(),
                status: Status::Fail,
                detail: Some(format!("/proc/self/status: {e}")),
            },
        }
    }

    fn check_memory_peak(release: Option<&str>) -> Outcome {
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

    fn check_setgroups() -> Outcome {
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
        let mut r = Report {
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

        r.outcomes.push(Outcome {
            name: "warn".into(),
            status: Status::Warn,
            detail: None,
        });
        let out = format!("{r}");
        assert!(out.contains("1 warning."));

        r.outcomes.push(Outcome {
            name: "fail".into(),
            status: Status::Fail,
            detail: Some("d".into()),
        });
        let out = format!("{r}");
        assert!(out.contains("Sandbox is NOT functional"));
        assert!(out.contains("1 failure"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_kernel_version_basic() {
        use super::linux::parse_kernel_version;
        assert_eq!(parse_kernel_version("5.10.0"), Some((5, 10)));
        assert_eq!(parse_kernel_version("6.6.87.2-microsoft"), Some((6, 6)));
        assert_eq!(parse_kernel_version("6.10-rc2"), Some((6, 10)));
        assert_eq!(parse_kernel_version("not.a.version"), None);
        assert_eq!(parse_kernel_version(""), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_run_produces_seven_outcomes() {
        let report = run();
        assert_eq!(report.os, "linux");
        assert_eq!(report.outcomes.len(), 7);
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn non_linux_run_reports_unsupported() {
        let report = run();
        assert!(!report.is_functional());
        assert_eq!(report.outcomes.len(), 1);
    }
}
