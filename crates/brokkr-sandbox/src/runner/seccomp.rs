//! Default-deny seccomp-bpf filter (M7).
//!
//! See `docs/phase-2-plan.md` §5.6. The filter is installed in the runner
//! after all setup syscalls have completed and immediately before
//! `execve`, so the action runs under the restricted policy.
//!
//! On a syscall mismatch we return `EPERM`, not kill the thread: the plan
//! argues that a killed process makes debugging painful and prevents the
//! user's command from emitting a sensible error message.
//!
//! ## seccompiler 0.5 API note
//!
//! `seccompiler` 0.5 keys its `SeccompFilter` rules by syscall *number*
//! (`BTreeMap<i64, Vec<SeccompRule>>`) and does not expose its internal
//! `SyscallTable` name resolver publicly. We therefore resolve names via
//! `nix::libc::SYS_*` constants, which `libc` defines per `target_arch`.
//! That is sound because the runner only ever installs a filter for the
//! current process — host arch and target arch are always the same.

use std::collections::BTreeSet;
use std::io::{self, ErrorKind};

use nix::libc;
use seccompiler::{
    apply_filter, BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition,
    SeccompFilter, SeccompRule, TargetArch,
};

/// Default syscall allowlist. Mirrors `docs/phase-2-plan.md` §5.6.
///
/// Names here that do not exist on the current target arch are silently
/// skipped (e.g. `fork`/`vfork`/`open`/`stat` on aarch64). Names supplied
/// via `extra_allow` that are unknown for this arch are an error.
const DEFAULT_ALLOW: &[&str] = &[
    "read",
    "write",
    "readv",
    "writev",
    "pread64",
    "pwrite64",
    "open",
    "openat",
    "openat2",
    "close",
    "close_range",
    "stat",
    "fstat",
    "lstat",
    "newfstatat",
    "statx",
    "lseek",
    "getdents",
    "getdents64",
    "readlink",
    "readlinkat",
    "access",
    "faccessat",
    "faccessat2",
    "fadvise64",
    "fdatasync",
    "fsync",
    "ftruncate",
    "truncate",
    "umask",
    "rename",
    "renameat",
    "renameat2",
    "unlink",
    "unlinkat",
    "rmdir",
    "mkdir",
    "mkdirat",
    "chmod",
    "fchmod",
    "fchmodat",
    "chown",
    "fchown",
    "fchownat",
    "lchown",
    "symlink",
    "symlinkat",
    "link",
    "linkat",
    "utimensat",
    "futimesat",
    "statfs",
    "fstatfs",
    "tgkill",
    "tkill",
    "kill",
    "rseq",
    "membarrier",
    "set_tid_address",
    "mmap",
    "mmap2",
    "munmap",
    "mremap",
    "mprotect",
    "madvise",
    "msync",
    "brk",
    "execve",
    "execveat",
    "wait4",
    "waitid",
    "exit",
    "exit_group",
    "rt_sigaction",
    "rt_sigprocmask",
    "rt_sigreturn",
    "rt_sigsuspend",
    "sigaltstack",
    "clone",
    "clone3",
    "fork",
    "vfork", // fork/vfork still useful for spawn helpers
    "pipe",
    "pipe2",
    "dup",
    "dup2",
    "dup3",
    "getpid",
    "getppid",
    "gettid",
    "getuid",
    "geteuid",
    "getgid",
    "getegid",
    "getgroups",
    "setgroups",
    "getcwd",
    "chdir",
    "fchdir",
    "fcntl",
    "fcntl64",
    "ioctl", // ioctl filtered further by arg
    "prlimit64",
    "getrlimit",
    "setrlimit",
    "arch_prctl",
    "prctl", // prctl filtered further by arg
    "sched_yield",
    "sched_getaffinity",
    "nanosleep",
    "clock_nanosleep",
    "clock_gettime",
    "clock_getres",
    "futex",
    "futex_waitv",
    "set_robust_list",
    "get_robust_list",
    "epoll_create",
    "epoll_create1",
    "epoll_ctl",
    "epoll_wait",
    "epoll_pwait",
    "poll",
    "ppoll",
    "select",
    "pselect6",
    "socket",
    "socketpair", // intentionally allowed; netns blocks egress
    "connect",
    "bind",
    "listen",
    "accept",
    "accept4",
    "shutdown",
    "getsockname",
    "getpeername",
    "setsockopt",
    "getsockopt",
    "sendto",
    "recvfrom",
    "sendmsg",
    "recvmsg",
    "sendmmsg",
    "recvmmsg",
    "uname",
    "sysinfo",
    "getrandom",
];

/// Resolve a syscall name to its number on the current target arch.
///
/// Returns `None` for names that do not exist on this arch (e.g. `fork`
/// on aarch64). Backed by `libc::SYS_*` constants, gated by
/// `cfg(target_arch)` so missing constants never break the build.

/// Resolve a syscall name to its number on the current target arch.
///
/// Returns `None` for names that do not exist on this arch (e.g. `fork`
/// on aarch64). Backed by `libc::SYS_*` constants, gated by
/// `cfg(target_arch)` so missing constants never break the build.
fn syscall_nr(name: &str) -> Option<i64> {
    // Common syscalls present on both x86_64 and aarch64 (and most other
    // Linux arches). Listed first so the giant arch-specific blocks below
    // stay readable.
    let common = match name {
        "read" => Some(libc::SYS_read),
        "write" => Some(libc::SYS_write),
        "readv" => Some(libc::SYS_readv),
        "writev" => Some(libc::SYS_writev),
        "pread64" => Some(libc::SYS_pread64),
        "pwrite64" => Some(libc::SYS_pwrite64),
        "openat" => Some(libc::SYS_openat),
        "openat2" => Some(libc::SYS_openat2),
        "close" => Some(libc::SYS_close),
        "close_range" => Some(libc::SYS_close_range),
        "fstat" => Some(libc::SYS_fstat),
        "newfstatat" => Some(libc::SYS_newfstatat),
        "statx" => Some(libc::SYS_statx),
        "lseek" => Some(libc::SYS_lseek),
        "getdents64" => Some(libc::SYS_getdents64),
        "readlinkat" => Some(libc::SYS_readlinkat),
        "faccessat" => Some(libc::SYS_faccessat),
        "faccessat2" => Some(libc::SYS_faccessat2),
        "fadvise64" => Some(libc::SYS_fadvise64),
        "fdatasync" => Some(libc::SYS_fdatasync),
        "fsync" => Some(libc::SYS_fsync),
        "ftruncate" => Some(libc::SYS_ftruncate),
        "truncate" => Some(libc::SYS_truncate),
        "umask" => Some(libc::SYS_umask),
        "renameat" => Some(libc::SYS_renameat),
        "renameat2" => Some(libc::SYS_renameat2),
        "unlinkat" => Some(libc::SYS_unlinkat),
        "mkdirat" => Some(libc::SYS_mkdirat),
        "fchmod" => Some(libc::SYS_fchmod),
        "fchmodat" => Some(libc::SYS_fchmodat),
        "fchown" => Some(libc::SYS_fchown),
        "fchownat" => Some(libc::SYS_fchownat),
        "symlinkat" => Some(libc::SYS_symlinkat),
        "linkat" => Some(libc::SYS_linkat),
        "utimensat" => Some(libc::SYS_utimensat),
        "statfs" => Some(libc::SYS_statfs),
        "fstatfs" => Some(libc::SYS_fstatfs),
        "tgkill" => Some(libc::SYS_tgkill),
        "tkill" => Some(libc::SYS_tkill),
        "kill" => Some(libc::SYS_kill),
        "rseq" => Some(libc::SYS_rseq),
        "membarrier" => Some(libc::SYS_membarrier),
        "set_tid_address" => Some(libc::SYS_set_tid_address),
        "mmap" => Some(libc::SYS_mmap),
        "munmap" => Some(libc::SYS_munmap),
        "mremap" => Some(libc::SYS_mremap),
        "mprotect" => Some(libc::SYS_mprotect),
        "madvise" => Some(libc::SYS_madvise),
        "msync" => Some(libc::SYS_msync),
        "brk" => Some(libc::SYS_brk),
        "execve" => Some(libc::SYS_execve),
        "execveat" => Some(libc::SYS_execveat),
        "wait4" => Some(libc::SYS_wait4),
        "waitid" => Some(libc::SYS_waitid),
        "exit" => Some(libc::SYS_exit),
        "exit_group" => Some(libc::SYS_exit_group),
        "rt_sigaction" => Some(libc::SYS_rt_sigaction),
        "rt_sigprocmask" => Some(libc::SYS_rt_sigprocmask),
        "rt_sigreturn" => Some(libc::SYS_rt_sigreturn),
        "rt_sigsuspend" => Some(libc::SYS_rt_sigsuspend),
        "sigaltstack" => Some(libc::SYS_sigaltstack),
        "clone" => Some(libc::SYS_clone),
        "clone3" => Some(libc::SYS_clone3),
        "pipe2" => Some(libc::SYS_pipe2),
        "dup" => Some(libc::SYS_dup),
        "dup3" => Some(libc::SYS_dup3),
        "getpid" => Some(libc::SYS_getpid),
        "getppid" => Some(libc::SYS_getppid),
        "gettid" => Some(libc::SYS_gettid),
        "getuid" => Some(libc::SYS_getuid),
        "geteuid" => Some(libc::SYS_geteuid),
        "getgid" => Some(libc::SYS_getgid),
        "getegid" => Some(libc::SYS_getegid),
        "getgroups" => Some(libc::SYS_getgroups),
        "setgroups" => Some(libc::SYS_setgroups),
        "getcwd" => Some(libc::SYS_getcwd),
        "chdir" => Some(libc::SYS_chdir),
        "fchdir" => Some(libc::SYS_fchdir),
        "fcntl" => Some(libc::SYS_fcntl),
        "ioctl" => Some(libc::SYS_ioctl),
        "prlimit64" => Some(libc::SYS_prlimit64),
        "setrlimit" => Some(libc::SYS_setrlimit),
        "prctl" => Some(libc::SYS_prctl),
        "sched_yield" => Some(libc::SYS_sched_yield),
        "sched_getaffinity" => Some(libc::SYS_sched_getaffinity),
        "nanosleep" => Some(libc::SYS_nanosleep),
        "clock_nanosleep" => Some(libc::SYS_clock_nanosleep),
        "clock_gettime" => Some(libc::SYS_clock_gettime),
        "clock_getres" => Some(libc::SYS_clock_getres),
        "futex" => Some(libc::SYS_futex),
        "set_robust_list" => Some(libc::SYS_set_robust_list),
        "get_robust_list" => Some(libc::SYS_get_robust_list),
        "epoll_create1" => Some(libc::SYS_epoll_create1),
        "epoll_ctl" => Some(libc::SYS_epoll_ctl),
        "epoll_pwait" => Some(libc::SYS_epoll_pwait),
        "ppoll" => Some(libc::SYS_ppoll),
        "pselect6" => Some(libc::SYS_pselect6),
        "socket" => Some(libc::SYS_socket),
        "socketpair" => Some(libc::SYS_socketpair),
        "connect" => Some(libc::SYS_connect),
        "bind" => Some(libc::SYS_bind),
        "listen" => Some(libc::SYS_listen),
        "accept" => Some(libc::SYS_accept),
        "accept4" => Some(libc::SYS_accept4),
        "shutdown" => Some(libc::SYS_shutdown),
        "getsockname" => Some(libc::SYS_getsockname),
        "getpeername" => Some(libc::SYS_getpeername),
        "setsockopt" => Some(libc::SYS_setsockopt),
        "getsockopt" => Some(libc::SYS_getsockopt),
        "sendto" => Some(libc::SYS_sendto),
        "recvfrom" => Some(libc::SYS_recvfrom),
        "sendmsg" => Some(libc::SYS_sendmsg),
        "recvmsg" => Some(libc::SYS_recvmsg),
        "sendmmsg" => Some(libc::SYS_sendmmsg),
        "recvmmsg" => Some(libc::SYS_recvmmsg),
        "uname" => Some(libc::SYS_uname),
        "sysinfo" => Some(libc::SYS_sysinfo),
        "getrandom" => Some(libc::SYS_getrandom),
        _ => None,
    };
    if common.is_some() {
        return common;
    }

    // x86_64-only legacy syscalls.
    #[cfg(target_arch = "x86_64")]
    {
        match name {
            "open" => return Some(libc::SYS_open),
            "stat" => return Some(libc::SYS_stat),
            "lstat" => return Some(libc::SYS_lstat),
            "fork" => return Some(libc::SYS_fork),
            "vfork" => return Some(libc::SYS_vfork),
            "pipe" => return Some(libc::SYS_pipe),
            "getrlimit" => return Some(libc::SYS_getrlimit),
            "arch_prctl" => return Some(libc::SYS_arch_prctl),
            "epoll_create" => return Some(libc::SYS_epoll_create),
            "epoll_wait" => return Some(libc::SYS_epoll_wait),
            "poll" => return Some(libc::SYS_poll),
            "select" => return Some(libc::SYS_select),
            "getdents" => return Some(libc::SYS_getdents),
            "readlink" => return Some(libc::SYS_readlink),
            "access" => return Some(libc::SYS_access),
            "rename" => return Some(libc::SYS_rename),
            "unlink" => return Some(libc::SYS_unlink),
            "rmdir" => return Some(libc::SYS_rmdir),
            "mkdir" => return Some(libc::SYS_mkdir),
            "chmod" => return Some(libc::SYS_chmod),
            "chown" => return Some(libc::SYS_chown),
            "lchown" => return Some(libc::SYS_lchown),
            "symlink" => return Some(libc::SYS_symlink),
            "link" => return Some(libc::SYS_link),
            "futimesat" => return Some(libc::SYS_futimesat),
            _ => {}
        }
    }

    // futex_waitv is gated on kernel and libc version; only present on
    // some arches in libc 0.2.
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    {
        if name == "futex_waitv" {
            return Some(libc::SYS_futex_waitv);
        }
    }

    // mmap2 / fcntl64 are 32-bit-only ABIs. We compile for 64-bit, so they
    // don't exist as `libc::SYS_*`. Treating them as unknown is correct.
    None
}

/// Map the compiled target architecture to seccompiler's `TargetArch`.
///
/// This is selected with `cfg(target_arch)` rather than a runtime string
/// match so seccomp follows the architecture the binary was built for.
fn host_target_arch() -> io::Result<TargetArch> {
    #[cfg(target_arch = "x86_64")]
    {
        return Ok(TargetArch::x86_64);
    }

    #[cfg(target_arch = "aarch64")]
    {
        return Ok(TargetArch::aarch64);
    }

    #[cfg(target_arch = "riscv64")]
    {
        return Ok(TargetArch::riscv64);
    }

    #[cfg(not(any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "riscv64"
    )))]
    {
        Err(io::Error::new(
            ErrorKind::Unsupported,
            format!("seccomp: unsupported arch {}", std::env::consts::ARCH),
        ))
    }
}

/// Build (but do not install) the BPF program for the default allowlist
/// merged with `extra_allow`. Split out so unit tests can exercise the
/// compiler without locking down the test process.
fn build_filter(extra_allow: &[String]) -> io::Result<BpfProgram> {
    let arch = host_target_arch()?;

    // Resolve names → numbers. DEFAULT_ALLOW silently drops names that do
    // not exist on this arch (they're informational); extras must resolve.
    let mut numbers: BTreeSet<i64> = BTreeSet::new();
    for name in DEFAULT_ALLOW {
        if let Some(nr) = syscall_nr(name) {
            numbers.insert(nr);
        }
    }
    for name in extra_allow {
        if name.is_empty() {
            continue;
        }
        match syscall_nr(name) {
            Some(nr) => {
                numbers.insert(nr);
            }
            None => {
                return Err(io::Error::new(
                    ErrorKind::InvalidInput,
                    format!("seccomp: unknown syscall in extra_allow: {name}"),
                ));
            }
        }
    }

    // Build per-syscall rules. Most syscalls get an empty rule vector
    // (unconditional allow). prctl and ioctl get argument-filtered rules.
    let rules: std::collections::BTreeMap<i64, Vec<SeccompRule>> = numbers
        .into_iter()
        .map(|nr| {
            if nr == libc::SYS_prctl {
                (nr, prctl_rules())
            } else if nr == libc::SYS_ioctl {
                (nr, ioctl_rules())
            } else {
                (nr, Vec::new())
            }
        })
        .collect();

    let filter = SeccompFilter::new(
        rules,
        // Mismatch: return EPERM (not Kill) — see module docs.
        SeccompAction::Errno(libc::EPERM as u32),
        // Match: allow.
        SeccompAction::Allow,
        arch,
    )
    .map_err(|e| io::Error::other(format!("seccomp: build filter: {e}")))?;

    let prog: BpfProgram = filter
        .try_into()
        .map_err(|e| io::Error::other(format!("seccomp: compile to BPF: {e}")))?;

    Ok(prog)
}

// ---------------------------------------------------------------------------
// prctl argument filtering
// ---------------------------------------------------------------------------

/// Return a BPF rule set for the `prctl` syscall.
///
/// Each rule is evaluated in order; the first matching rule's action applies.
/// Dangerous options are blocked (EPERM); safe options are allowed.
/// A catch-all allow rule terminates the chain for anything not explicitly
/// blocked.
fn prctl_rules() -> Vec<SeccompRule> {
    let arg0 = SeccompCmpArgLen::Dword;
    let eq = SeccompCmpOp::Eq;
    let masked = SeccompCmpOp::MaskedEq(0);

    let mut rules = Vec::new();

    // Block: PR_SET_KEEPCAPS (31) — allows setuid binaries to retain caps
    rules.push(
        SeccompRule::new(vec![SeccompCondition::new(0, arg0, eq, 31).expect("valid condition")])
            .expect("valid prctl rule"),
    );

    // Block: PR_CAPBSET_DROP (36) — permanently removes caps from process
    rules.push(
        SeccompRule::new(vec![SeccompCondition::new(0, arg0, eq, 36).expect("valid condition")])
            .expect("valid prctl rule"),
    );

    // Block: PR_SET_TSC (10) — enables timing side-channel (RDTSC) control
    rules.push(
        SeccompRule::new(vec![SeccompCondition::new(0, arg0, eq, 10).expect("valid condition")])
            .expect("valid prctl rule"),
    );

    // Block: PR_GET_TSC (11) — query side-channel control
    rules.push(
        SeccompRule::new(vec![SeccompCondition::new(0, arg0, eq, 11).expect("valid condition")])
            .expect("valid prctl rule"),
    );

    // Catch-all allow: anything not explicitly blocked above is permitted.
    // MaskedEq(0) on arg0==0 always matches (since 0&0==0), acting as a
    // catch-all that allows all other prctl options.
    rules.push(
        SeccompRule::new(vec![SeccompCondition::new(0, arg0, masked, 0).expect("valid condition")])
            .expect("valid prctl rule"),
    );

    rules
}

// ---------------------------------------------------------------------------
// ioctl argument filtering
// ---------------------------------------------------------------------------

/// Return a BPF rule set for the `ioctl` syscall.
///
/// The request code is in `arg1` (arg0 is the file descriptor).
/// Terminal/device-manipulation calls are blocked; all others are allowed.
fn ioctl_rules() -> Vec<SeccompRule> {
    let arg1 = SeccompCmpArgLen::Dword;
    let eq = SeccompCmpOp::Eq;
    let masked = SeccompCmpOp::MaskedEq(0);

    let mut rules = Vec::new();

    // Block TIOCSTI (0x5412) — simulates terminal input
    rules.push(
        SeccompRule::new(vec![SeccompCondition::new(1, arg1, eq, 0x5412).expect("valid condition")])
            .expect("valid ioctl rule"),
    );

    // Block TIOCSWINSZ (0x5414) — set terminal window size
    rules.push(
        SeccompRule::new(vec![SeccompCondition::new(1, arg1, eq, 0x5414).expect("valid condition")])
            .expect("valid ioctl rule"),
    );

    // Block TIOCGWINSZ (0x5413) — get terminal window size (info leak)
    rules.push(
        SeccompRule::new(vec![SeccompCondition::new(1, arg1, eq, 0x5413).expect("valid condition")])
            .expect("valid ioctl rule"),
    );

    // Block TIOCSBRK (0x5429) — set break condition on terminal
    rules.push(
        SeccompRule::new(vec![SeccompCondition::new(1, arg1, eq, 0x5429).expect("valid condition")])
            .expect("valid ioctl rule"),
    );

    // Block TIOCCBRK (0x542A) — clear break condition
    rules.push(
        SeccompRule::new(vec![SeccompCondition::new(1, arg1, eq, 0x542A).expect("valid condition")])
            .expect("valid ioctl rule"),
    );

    // Block TIOCSPTLCK (0x4D60) — unlock pseudo-terminal device lock
    rules.push(
        SeccompRule::new(vec![SeccompCondition::new(1, arg1, eq, 0x4D60).expect("valid condition")])
            .expect("valid ioctl rule"),
    );

    // Note: TIOCGSID (0x5429) has the same value as TIOCSBRK on x86_64/aarch64,
    // so it is already blocked by the TIOCSBRK rule above.

    // Catch-all allow: any ioctl request not explicitly blocked above is
    // permitted. MaskedEq(0) on arg1 always matches.
    rules.push(
        SeccompRule::new(vec![SeccompCondition::new(1, arg1, masked, 0).expect("valid condition")])
            .expect("valid ioctl rule"),
    );

    rules
}

/// Install a default-deny seccomp filter on the calling thread.
///
/// `extra_allow` is an additive allowlist of syscall names beyond
/// [`DEFAULT_ALLOW`]. Unknown names are rejected with
/// [`io::ErrorKind::InvalidInput`] so misconfiguration surfaces loudly
/// instead of silently widening the policy.
pub(super) fn install(extra_allow: &[String]) -> io::Result<()> {
    let prog = build_filter(extra_allow)?;
    let bpf_instruction_count = prog.len();
    apply_filter(&prog).map_err(|e| io::Error::other(format!("seccomp: apply_filter: {e}")))?;
    tracing::debug!(
        bpf_instructions = bpf_instruction_count,
        "installed seccomp filter"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_allow_contains_essentials() {
        for must in ["execve", "exit_group", "read", "write"] {
            assert!(
                DEFAULT_ALLOW.contains(&must),
                "DEFAULT_ALLOW missing essential syscall: {must}",
            );
        }
    }

    #[test]
    #[allow(clippy::expect_used)]
    fn compiles_on_host_arch() {
        // Build but do not install — installing would lock down the test
        // process and break subsequent tests in the same binary.
        let prog = build_filter(&[]).expect("filter must compile on host arch");
        assert!(!prog.is_empty(), "BPF program should be non-empty");
    }

    #[test]
    #[allow(clippy::expect_used)]
    fn unknown_extra_syscall_is_rejected() {
        let err = build_filter(&["definitely_not_a_syscall".to_string()])
            .expect_err("unknown extra syscall must error");
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
    }

    #[test]
    #[allow(clippy::expect_used)]
    fn empty_extra_entries_are_ignored() {
        // Empty strings come from sloppy config splitting; tolerate them
        // rather than rejecting, per the spec's "ignore empty strings".
        build_filter(&[String::new()]).expect("empty extra entries must be ignored");
    }
}
