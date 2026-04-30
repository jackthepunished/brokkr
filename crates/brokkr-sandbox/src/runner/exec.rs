//! Linux runner main: read config, set up namespaces / rootfs, exec.
//!
//! M2 ran the action with no isolation. M3 inserts user + mount
//! namespace setup; M4 adds a PID namespace and a fork-and-reap step:
//!
//! ```text
//! brokkr-sandboxd (host pidns)
//!   └─ unshare(NEWUSER|NEWNS|NEWPID), setup_rootfs, then fork
//!        ├─ outer runner: waitpid(init); mirror init's exit
//!        └─ init = PID 1 in new pidns: mount /proc, fork
//!             ├─ init: reap orphans, waitpid(action); mirror its exit
//!             └─ action = PID 2: chdir(workdir); execvpe(argv)
//! ```
//!
//! The whole namespace path is gated on whether the caller asked for a
//! rootfs: a default-empty [`RootfsSpec`] keeps the M2 path so existing
//! smoke tests don't break.

use std::ffi::CString;
use std::io::Read as _;
use std::os::fd::{FromRawFd, RawFd};

use nix::sys::wait::waitpid;
use nix::unistd::{fork, ForkResult};

use crate::config::SandboxConfig;

use super::mount::setup_rootfs;
use super::pidns::{exit_with, mount_proc, reap_until};
use super::userns::{setup_namespaces, UidGidMap};
use super::{die, errno_message};

/// File descriptor on which the host writes the JSON-encoded
/// `SandboxConfig`. Hard-coded by convention on both sides; see
/// `docs/phase-2-plan.md` §3.3.
const CONFIG_FD: RawFd = 3;

pub(super) fn runner_main() -> ! {
    let cfg = match read_config() {
        Ok(c) => c,
        Err(e) => die("failed to read config", &e.to_string()),
    };

    if cfg.rootfs.is_empty() {
        // M2 path: no isolation, run the action in-process.
        chdir_and_exec(&cfg);
    }

    let map = UidGidMap {
        host_uid: nix::unistd::getuid().as_raw(),
        host_gid: nix::unistd::getgid().as_raw(),
    };
    if let Err(e) = setup_namespaces(map) {
        die("setup namespaces", &e.to_string());
    }
    if let Err(e) = setup_rootfs(&cfg.rootfs) {
        die("setup rootfs", &e.to_string());
    }

    // First fork: child becomes PID 1 in the new PID namespace.
    //
    // SAFETY: fork is async-signal-safe; we touch no shared mutable
    // state between branches before either waitpid (parent) or
    // run_init_then_exec (child).
    #[allow(unsafe_code)]
    let init_fork = unsafe { fork() };
    match init_fork {
        Err(errno) => die("fork init", &errno_message(errno)),
        Ok(ForkResult::Parent { child }) => match waitpid(child, None) {
            Ok(status) => exit_with(status),
            Err(errno) => die("waitpid init", &errno_message(errno)),
        },
        Ok(ForkResult::Child) => run_init_then_exec(cfg),
    }
}

/// PID 1 of the sandbox PID namespace. Mounts `/proc`, forks the action
/// child (PID 2), and either reaps until the action exits (parent) or
/// chdir+execs the action (child). Always diverges.
fn run_init_then_exec(cfg: SandboxConfig) -> ! {
    if let Err(e) = mount_proc() {
        die("mount /proc inside pidns", &e.to_string());
    }

    // Second fork: child becomes PID 2 (the action), parent stays as
    // PID 1 (the reaper).
    //
    // SAFETY: see init_fork above.
    #[allow(unsafe_code)]
    let action_fork = unsafe { fork() };
    match action_fork {
        Err(errno) => die("fork action", &errno_message(errno)),
        Ok(ForkResult::Parent { child }) => reap_until(child),
        Ok(ForkResult::Child) => chdir_and_exec(&cfg),
    }
}

/// Final step of the action child: chdir into the configured workdir
/// (if any) and `execvpe` the action. Diverges; on any setup failure
/// it calls `die(...)` which prints a diagnostic and terminates via
/// `std::process::exit(127)`.
fn chdir_and_exec(cfg: &SandboxConfig) -> ! {
    if let Some(workdir) = &cfg.workdir {
        if let Err(e) = std::env::set_current_dir(workdir) {
            die("failed to chdir into workdir", &e.to_string());
        }
    }

    let argv = match build_argv(&cfg.argv) {
        Ok(a) => a,
        Err(msg) => die("invalid argv", msg),
    };
    let env = match build_env(&cfg.env) {
        Ok(e) => e,
        Err(msg) => die("invalid env", msg),
    };

    let argv_refs: Vec<&CString> = argv.iter().collect();
    let env_refs: Vec<&CString> = env.iter().collect();

    match nix::unistd::execvpe(&argv[0], &argv_refs, &env_refs) {
        Ok(_) => unreachable!("execvpe returned Ok"),
        Err(errno) => die(
            "execvpe failed",
            &format!("{}: {}", cfg.argv[0], errno_message(errno)),
        ),
    }
}

fn read_config() -> std::io::Result<SandboxConfig> {
    // SAFETY: the host places the config pipe's read end on CONFIG_FD via
    // dup2 in pre_exec, and never opens any other fd at that number. We
    // become its sole owner.
    #[allow(unsafe_code)]
    let mut file = unsafe { std::fs::File::from_raw_fd(CONFIG_FD) };
    let mut buf = Vec::with_capacity(4096);
    file.read_to_end(&mut buf)?;
    let cfg: SandboxConfig = serde_json::from_slice(&buf)
        .map_err(|e| std::io::Error::other(format!("config json: {e}")))?;
    Ok(cfg)
}

fn build_argv(argv: &[String]) -> Result<Vec<CString>, &'static str> {
    if argv.is_empty() {
        return Err("argv is empty");
    }
    argv.iter()
        .map(|s| CString::new(s.as_bytes()).map_err(|_| "argv entry contains NUL"))
        .collect()
}

fn build_env(env: &[(String, String)]) -> Result<Vec<CString>, &'static str> {
    env.iter()
        .map(|(k, v)| {
            if k.contains('=') || k.contains('\0') || v.contains('\0') {
                Err("env entry contains NUL or '='")
            } else {
                CString::new(format!("{k}={v}")).map_err(|_| "env entry contains NUL")
            }
        })
        .collect()
}
