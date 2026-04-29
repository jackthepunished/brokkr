//! Linux runner main: read config, set up namespaces / rootfs, exec.
//!
//! M2 ran the action with no isolation. M3 inserts user-namespace setup
//! and mount-namespace + `pivot_root` work between the config read and
//! the final `execvpe`, gated on whether the caller asked for a rootfs:
//! a default-empty [`RootfsSpec`] keeps the M2 path so existing smoke
//! tests don't break.

use std::ffi::CString;
use std::io::Read as _;
use std::os::fd::{FromRawFd, RawFd};

use crate::config::SandboxConfig;

use super::mount::setup_rootfs;
use super::userns::{setup_user_and_mount_namespaces, UidGidMap};

/// File descriptor on which the host writes the JSON-encoded
/// `SandboxConfig`. Hard-coded by convention on both sides; see
/// `docs/phase-2-plan.md` §3.3.
const CONFIG_FD: RawFd = 3;

pub(super) fn runner_main() -> ! {
    let cfg = match read_config() {
        Ok(c) => c,
        Err(e) => die("failed to read config", &e.to_string()),
    };

    if !cfg.rootfs.is_empty() {
        let map = UidGidMap {
            host_uid: nix::unistd::getuid().as_raw(),
            host_gid: nix::unistd::getgid().as_raw(),
        };
        if let Err(e) = setup_user_and_mount_namespaces(map) {
            die("setup user/mount namespaces", &e.to_string());
        }
        if let Err(e) = setup_rootfs(&cfg.rootfs) {
            die("setup rootfs", &e.to_string());
        }
    }

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

fn errno_message(errno: nix::errno::Errno) -> String {
    format!("{} ({})", errno.desc(), errno as i32)
}

fn die(step: &str, message: &str) -> ! {
    // Best-effort write to stderr; if even that fails we just exit.
    eprintln!("brokkr-sandboxd: {step}: {message}");
    std::process::exit(127);
}
