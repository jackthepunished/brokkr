//! Linux-only host check implementations.
//!
//! Each check lives in its own file. The `run_linux()` aggregator is also
//! here so it can call into each check without going through `super`.

#[cfg(target_os = "linux")]
mod brokkr_slice;
#[cfg(target_os = "linux")]
mod cgroup2;
#[cfg(target_os = "linux")]
pub(crate) mod kernel_version;
#[cfg(target_os = "linux")]
pub(crate) mod memory_peak;
#[cfg(target_os = "linux")]
mod seccomp;
#[cfg(target_os = "linux")]
mod setgroups;
#[cfg(target_os = "linux")]
pub(crate) mod subtree_controllers;
#[cfg(target_os = "linux")]
mod user_namespaces;

#[cfg(target_os = "linux")]
/// Run all Linux-only host check functions and return their outcomes.
pub fn run_linux(kernel_release: Option<&str>) -> Vec<super::Outcome> {
    vec![
        kernel_version::check_kernel_version(kernel_release),
        user_namespaces::check_user_namespaces(),
        cgroup2::check_cgroup_v2(),
        brokkr_slice::check_brokkr_slice(),
        subtree_controllers::check_subtree_controllers(),
        seccomp::check_seccomp(),
        memory_peak::check_memory_peak(kernel_release),
        setgroups::check_setgroups(),
    ]
}
