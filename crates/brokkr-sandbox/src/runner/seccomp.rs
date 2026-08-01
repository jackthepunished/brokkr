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
//!
//! Crucially, a `SeccompFilter` has exactly *one* match action for the whole
//! filter (plus one default action for non-matching syscalls). A rule that
//! matches always yields that single match action — there is no per-rule
//! action. The base filter uses match = `Allow` (an allowlist), which means
//! it *cannot* express "allow `ioctl` except when arg1 is `TIOCSTI`": any such
//! rule would return `Allow`. Per-argument denials therefore live in a
//! *second, stacked* filter ([`build_deny_filter`]) whose match action is
//! `Errno(EPERM)`. The kernel runs every installed filter and keeps the most
//! restrictive verdict, so the pair composes to a true allow-with-exceptions
//! policy.

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
    "ioctl", // allowed here; dangerous request codes denied by build_deny_filter
    "prlimit64",
    "getrlimit",
    "setrlimit",
    "arch_prctl",
    "prctl", // allowed here; dangerous options denied by build_deny_filter
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
    // Sockets are allowed, but the action cannot use them to reach anything
    // outside the sandbox or smuggle fds across its boundary (issue #69):
    //   * It runs in a fresh network namespace (NetworkPolicy::None leaves no
    //     routable interface), which also scopes *abstract* AF_UNIX sockets to
    //     the sandbox — they cannot name a host socket.
    //   * The mount namespace exposes no host *pathname* AF_UNIX sockets (the
    //     default rootfs is ro /usr + tmpfs), so connect() has no external
    //     endpoint to reach.
    //   * socketpair() has no external endpoint at all — both ends belong to
    //     the action — so an SCM_RIGHTS cmsg over it only passes fds between
    //     the action's own processes, which fork already permits. Cross-
    //     boundary fd smuggling needs an external socket endpoint, and the
    //     namespaces above guarantee there is none.
    "socket",
    "socketpair",
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
        // `SYS_fadvise64` is exposed on x86_64 / riscv64 but not on
        // aarch64 in `libc` (the aarch64 ABI calls the syscall
        // `arm64_fadvise64_64` internally; the `libc` crate hasn't added
        // a constant for it). Skip silently on arches that don't expose
        // a usable constant — `resolve_or_skip` treats `None` from this
        // helper as "syscall absent on this arch".
        #[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
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
        Ok(TargetArch::x86_64)
    }
    #[cfg(target_arch = "aarch64")]
    {
        Ok(TargetArch::aarch64)
    }
    #[cfg(target_arch = "riscv64")]
    {
        Ok(TargetArch::riscv64)
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

    // Base filter: every allowlisted syscall is permitted unconditionally.
    // Argument-level denials for `prctl`/`ioctl` cannot live here — seccompiler
    // applies a single match action per filter, so a rule matching a dangerous
    // argument would yield this filter's match action (`Allow`), not a denial.
    // Those denials are enforced by a second, stacked filter; see
    // [`build_deny_filter`].
    let rules: std::collections::BTreeMap<i64, Vec<SeccompRule>> =
        numbers.into_iter().map(|nr| (nr, Vec::new())).collect();

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

/// Build (but do not install) the *argument-denial* BPF program.
///
/// The base filter ([`build_filter`]) permits `prctl` and `ioctl`
/// unconditionally. This second filter is stacked on top of it and returns
/// `EPERM` when a dangerous argument value matches, `Allow` otherwise.
///
/// The kernel evaluates every installed seccomp filter and applies the most
/// restrictive result (`SECCOMP_RET_ERRNO` outranks `SECCOMP_RET_ALLOW`), so
/// the stacked pair reads as "allow the syscall except for these argument
/// values" — a policy a single seccompiler filter cannot express (its one
/// match action would have to be both `Allow` and `Errno` at once). Splitting
/// it out is why [`build_filter`] leaves `prctl`/`ioctl` unconditionally
/// allowed.
fn build_deny_filter() -> io::Result<BpfProgram> {
    let arch = host_target_arch()?;

    let mut rules: std::collections::BTreeMap<i64, Vec<SeccompRule>> =
        std::collections::BTreeMap::new();
    rules.insert(libc::SYS_prctl, prctl_deny_rules());
    rules.insert(libc::SYS_ioctl, ioctl_deny_rules());

    let filter = SeccompFilter::new(
        rules,
        // Default action (argument not in a deny rule): allow, deferring to
        // the base filter for the accept/reject decision.
        SeccompAction::Allow,
        // Match action (a denied argument value): reject with EPERM.
        SeccompAction::Errno(libc::EPERM as u32),
        arch,
    )
    .map_err(|e| io::Error::other(format!("seccomp: build deny filter: {e}")))?;

    filter
        .try_into()
        .map_err(|e| io::Error::other(format!("seccomp: compile deny filter to BPF: {e}")))
}

// ---------------------------------------------------------------------------
// prctl argument filtering
// ---------------------------------------------------------------------------

/// Argument-denial rules for `prctl`, consumed by [`build_deny_filter`].
///
/// Each rule matches one dangerous `option` value (arg0); a match denies the
/// call with `EPERM`. Any option not listed here falls through to the deny
/// filter's default `Allow` and is governed by the base allowlist.
///
/// Values come from `libc`, which resolves them per target arch. The previous
/// hand-written literals (31/36/10/11) named the wrong options entirely —
/// `PR_TASK_PERF_EVENTS_DISABLE`, `PR_SET_CHILD_SUBREAPER`, `PR_SET_FPEMU`,
/// `PR_GET_FPEXC` — so even a correctly-actioned filter would have denied the
/// wrong things.
#[allow(clippy::expect_used)]
fn prctl_deny_rules() -> Vec<SeccompRule> {
    const DENIED_PRCTL_OPTIONS: &[libc::c_int] = &[
        libc::PR_SET_KEEPCAPS, // 8  — let a setuid exec retain capabilities
        libc::PR_CAPBSET_DROP, // 24 — mutate the capability bounding set
        libc::PR_SET_TSC,      // 26 — re-enable the timestamp counter (RDTSC)
        libc::PR_GET_TSC,      // 25 — probe timestamp-counter state (side channel)
    ];
    DENIED_PRCTL_OPTIONS
        .iter()
        .map(|&opt| {
            SeccompRule::new(vec![SeccompCondition::new(
                0,
                SeccompCmpArgLen::Dword,
                SeccompCmpOp::Eq,
                opt as u64,
            )
            .expect("valid prctl condition")])
            .expect("valid prctl deny rule")
        })
        .collect()
}

// ---------------------------------------------------------------------------
// ioctl argument filtering
// ---------------------------------------------------------------------------

/// Argument-denial rules for `ioctl`, consumed by [`build_deny_filter`].
///
/// The request code is in arg1 (arg0 is the file descriptor). Each rule
/// matches one dangerous request; a match denies with `EPERM`. Anything else
/// falls through to the deny filter's default `Allow` and is governed by the
/// base allowlist. Request codes come from `libc` so they are correct for the
/// target arch (the previous `TIOCSPTLCK` literal `0x4D60` was also wrong).
//
// `req as u64` widens the `libc::Ioctl` request to the `u64` that
// `SeccompCondition::new` expects. On 64-bit targets `Ioctl` is already `u64`
// so the cast is a no-op there, but it is needed on arches where `Ioctl` is
// narrower — hence the local `unnecessary_cast` allow.
#[allow(clippy::expect_used, clippy::unnecessary_cast)]
fn ioctl_deny_rules() -> Vec<SeccompRule> {
    // Typed as `[libc::Ioctl; _]` by inference — the values carry the correct
    // per-arch request encoding without us naming the (per-target) alias.
    let denied_ioctl_requests = [
        libc::TIOCSTI,    // inject characters into a terminal's input queue
        libc::TIOCSWINSZ, // set terminal window size
        libc::TIOCGWINSZ, // read terminal window size
        libc::TIOCSBRK,   // assert a break condition on the line
        libc::TIOCCBRK,   // clear a break condition on the line
        libc::TIOCSPTLCK, // (un)lock a pseudo-terminal slave
    ];
    denied_ioctl_requests
        .iter()
        .map(|&req| {
            SeccompRule::new(vec![SeccompCondition::new(
                1,
                SeccompCmpArgLen::Dword,
                SeccompCmpOp::Eq,
                req as u64,
            )
            .expect("valid ioctl condition")])
            .expect("valid ioctl deny rule")
        })
        .collect()
}

/// Install the sandbox's seccomp policy on the calling thread.
///
/// Two filters are stacked: the base allowlist ([`build_filter`], additively
/// widened by `extra_allow`) and the argument-denial overlay
/// ([`build_deny_filter`]). Unknown names in `extra_allow` are rejected with
/// [`io::ErrorKind::InvalidInput`] so misconfiguration surfaces loudly instead
/// of silently widening the policy.
pub(super) fn install(extra_allow: &[String]) -> io::Result<()> {
    let base = build_filter(extra_allow)?;
    let deny = build_deny_filter()?;
    let base_len = base.len();
    let deny_len = deny.len();
    // Install order matters: `seccomp` itself is NOT in the base allowlist, so
    // once the base filter is active a further `apply_filter` would be denied.
    // Install the deny overlay first (its default action is `Allow`, so it
    // permits the base install), then the base allowlist last. Stacked filters
    // are evaluated independently, so the runtime semantics do not depend on
    // this order — only the bootstrap does.
    apply_filter(&deny)
        .map_err(|e| io::Error::other(format!("seccomp: apply_filter (deny): {e}")))?;
    apply_filter(&base)
        .map_err(|e| io::Error::other(format!("seccomp: apply_filter (base): {e}")))?;
    tracing::debug!(
        base_bpf_instructions = base_len,
        deny_bpf_instructions = deny_len,
        "installed seccomp filters"
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

        let deny = build_deny_filter().expect("deny filter must compile on host arch");
        assert!(!deny.is_empty(), "deny BPF program should be non-empty");
    }

    /// The stacked filters must actually deny the dangerous `prctl`/`ioctl`
    /// arguments while leaving benign ones alone.
    ///
    /// This installs the compiled filters in a *forked child* (so the parent
    /// test process stays unrestricted) after `PR_SET_NO_NEW_PRIVS`, which
    /// lets an unprivileged process install seccomp with no user namespace or
    /// elevated privilege. It therefore runs on ordinary CI and does **not**
    /// skip — unlike the end-to-end `evil_seccomp_caps` tests, which need
    /// unprivileged userns and previously masked this bug by skipping.
    #[cfg(target_os = "linux")]
    #[test]
    #[allow(clippy::expect_used, unsafe_code)]
    fn stacked_filters_deny_dangerous_args_and_allow_benign() {
        let base = build_filter(&[]).expect("base filter compiles");
        let deny = build_deny_filter().expect("deny filter compiles");

        // SAFETY: `fork` in a test. The child path only calls
        // async-signal-safe libc functions and `_exit`, and touches no shared
        // allocator state that the parent relies on.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork failed: {}", io::Error::last_os_error());

        if pid == 0 {
            // Child: lock down, then probe. Failures are encoded as bits in
            // the exit code (0 == all expectations met).
            // SAFETY: raw prctl/seccomp calls affect only this thread.
            unsafe {
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                    libc::_exit(100);
                }
            }
            // Deny overlay first, then the base allowlist — mirrors `install`
            // (the base filter blocks the `seccomp` syscall, so it must be
            // installed last).
            if apply_filter(&deny).is_err() {
                // SAFETY: terminate the child without unwinding.
                unsafe { libc::_exit(101) }
            }
            if apply_filter(&base).is_err() {
                // SAFETY: as above.
                unsafe { libc::_exit(102) }
            }

            let mut mask: i32 = 0;

            // PR_SET_KEEPCAPS must be denied with EPERM.
            // SAFETY: prctl with scalar arguments.
            let rc = unsafe { libc::prctl(libc::PR_SET_KEEPCAPS, 0, 0, 0, 0) };
            if !(rc == -1 && io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)) {
                mask |= 1;
            }

            // PR_SET_NAME is benign and must still succeed.
            let name = b"brokkr-sbx\0";
            // SAFETY: `name` is NUL-terminated; prctl reads a <=16-byte string.
            let rc = unsafe { libc::prctl(libc::PR_SET_NAME, name.as_ptr(), 0, 0, 0) };
            if rc != 0 {
                mask |= 2;
            }

            // ioctl(TIOCSTI) must be denied with EPERM regardless of the fd:
            // seccomp rejects it before the kernel ever inspects the fd, so a
            // non-EPERM errno (e.g. ENOTTY) means the filter let it through.
            let mut ch: u8 = b'x';
            // SAFETY: ioctl on stderr with a byte pointer; denied pre-dispatch.
            let rc = unsafe { libc::ioctl(2, libc::TIOCSTI, &mut ch as *mut u8) };
            if !(rc == -1 && io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)) {
                mask |= 4;
            }

            // SAFETY: terminate the child without running atexit handlers.
            unsafe { libc::_exit(mask) }
        }

        // Parent: reap and assert the child reported no failures.
        let mut status: i32 = 0;
        // SAFETY: `waitpid` with a valid out pointer for the child we forked.
        let waited = unsafe { libc::waitpid(pid, &mut status, 0) };
        assert_eq!(
            waited,
            pid,
            "waitpid failed: {}",
            io::Error::last_os_error()
        );
        assert!(
            libc::WIFEXITED(status),
            "child did not exit normally (status={status})"
        );
        let code = libc::WEXITSTATUS(status);
        assert_eq!(
            code, 0,
            "child probe failed. codes 100-102 = setup failure; otherwise bitmask \
             bit0=PR_SET_KEEPCAPS not denied, bit1=PR_SET_NAME rejected, \
             bit2=ioctl(TIOCSTI) not denied (code={code})"
        );
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
