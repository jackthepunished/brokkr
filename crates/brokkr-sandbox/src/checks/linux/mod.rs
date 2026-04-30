//! Linux-only host checks.

use std::fs;

pub mod brokkr_slice;
pub mod cgroup2;
pub mod kernel_version;
pub mod memory_peak;
pub mod seccomp;
pub mod setgroups;
pub mod user_namespaces;

use crate::checks::Report;

/// Run all Linux-only host checks and return a [`Report`].
pub fn run(os: &'static str, arch: &'static str) -> Report {
    let kernel_release = fs::read_to_string("/proc/sys/kernel/osrelease")
        .ok()
        .map(|s| s.trim().to_string());

    let outcomes = vec![
        kernel_version::check_kernel_version(kernel_release.as_deref()),
        user_namespaces::check_user_namespaces(),
        cgroup2::check_cgroup_v2(),
        brokkr_slice::check_brokkr_slice(),
        seccomp::check_seccomp(),
        memory_peak::check_memory_peak(kernel_release.as_deref()),
        setgroups::check_setgroups(),
    ];

    Report {
        os,
        arch,
        kernel_release,
        outcomes,
    }
}
