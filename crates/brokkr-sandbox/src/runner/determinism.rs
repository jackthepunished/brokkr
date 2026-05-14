//! Determinism guards inside the runner (M8).
//!
//! See `docs/phase-2-plan.md` §5.8. Two halves:
//!
//! - **[`apply_pre_fork`]** runs once in the runner-as-PID-namespace-init
//!   (i.e. after [`super::userns::setup_namespaces`] has unshared
//!   `CLONE_NEWUTS`, after [`super::mount::setup_rootfs`] has laid down
//!   the tmpfs `/etc`, and before the action `fork`s). It calls
//!   `sethostname(2)` inside the new UTS namespace and symlinks
//!   `/etc/localtime` so glibc-based tools that consult zoneinfo see
//!   UTC.
//! - **[`scrub_env`]** runs in the action child immediately before
//!   `execve`. It returns the post-scrub env list to pass to
//!   `execvpe(2)`: `LD_PRELOAD` / `LD_LIBRARY_PATH` are filtered when
//!   `strip_ld_preload` is set, `PATH` is replaced when `strip_path`
//!   is set, and `TZ` / `SOURCE_DATE_EPOCH` are upserted when their
//!   corresponding knobs are configured.
//!
//! Pre-fork and pre-exec are deliberately split: hostname / symlink
//! work needs the synthetic `CAP_SYS_ADMIN` we get inside the user
//! namespace and the writable tmpfs `/etc`, neither of which exists
//! on the no-isolation (M2) path. Env scrubbing is pure data
//! manipulation, runs on both paths, and is the only piece a worker
//! actually wants to apply without namespaces.

use std::io;

use crate::config::DeterminismPolicy;

use super::nix_io;

/// Apply the hostname / symlink half of [`DeterminismPolicy`] inside
/// the runner. Must be called after the new UTS namespace exists and
/// after the rootfs `/etc` mount point is in place. Idempotent on the
/// `/etc/localtime` symlink: if a file is already there we overwrite
/// it; if the link target's directory doesn't exist (e.g. the rootfs
/// didn't bind `/usr/share/zoneinfo`), the symlink will dangle, which
/// glibc gracefully falls back from to the `TZ` env var.
pub(super) fn apply_pre_fork(policy: &DeterminismPolicy) -> io::Result<()> {
    if let Some(hostname) = &policy.hostname {
        if !hostname.is_empty() {
            // sethostname inside the new UTS namespace; affects only
            // the sandbox. Empty string is treated as "don't touch"
            // so callers can express the same thing via `None` or
            // `Some("")` without surprises.
            nix::unistd::sethostname(hostname.as_str()).map_err(nix_io)?;
        }
    }
    if policy.timezone_utc {
        // /etc is a tmpfs mounted by setup_rootfs — overwriting
        // anything that happened to be there is safe.
        let _ = std::fs::remove_file("/etc/localtime");
        // Best-effort: only fails if `/etc` is missing, which would
        // already have errored out earlier on the namespace path.
        std::os::unix::fs::symlink("/usr/share/zoneinfo/Etc/UTC", "/etc/localtime")?;
    }
    Ok(())
}

/// Return the env list to hand to `execve` after applying scrub
/// policies. Pure function over `(env, policy)` — no I/O.
///
/// Order of operations matters for one edge case: `strip_path` removes
/// any caller-supplied `PATH` *and* injects the fixed default, so a
/// later upsert of `TZ`/`SOURCE_DATE_EPOCH` cannot accidentally
/// re-introduce the caller's `PATH`.
pub(super) fn scrub_env(
    env: &[(String, String)],
    policy: &DeterminismPolicy,
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = env
        .iter()
        .filter(|(k, _)| {
            if policy.strip_ld_preload && (k == "LD_PRELOAD" || k == "LD_LIBRARY_PATH") {
                return false;
            }
            if policy.strip_path && k == "PATH" {
                return false;
            }
            true
        })
        .cloned()
        .collect();

    if policy.strip_path {
        out.push(("PATH".to_string(), "/usr/bin:/bin".to_string()));
    }
    if policy.timezone_utc {
        upsert(&mut out, "TZ", "UTC0");
    }
    if let Some(epoch) = policy.source_date_epoch {
        upsert(&mut out, "SOURCE_DATE_EPOCH", &epoch.to_string());
    }
    out
}

fn upsert(env: &mut Vec<(String, String)>, key: &str, val: &str) {
    for entry in env.iter_mut() {
        if entry.0 == key {
            entry.1 = val.to_string();
            return;
        }
    }
    env.push((key.to_string(), val.to_string()));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pol(strip_ld: bool, strip_path: bool, tz: bool, epoch: Option<i64>) -> DeterminismPolicy {
        DeterminismPolicy {
            hostname: None,
            timezone_utc: tz,
            source_date_epoch: epoch,
            strip_ld_preload: strip_ld,
            strip_path,
        }
    }

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn default_policy_is_passthrough() {
        let p = DeterminismPolicy::default();
        let e = env(&[("PATH", "/x"), ("LD_PRELOAD", "/evil.so"), ("KEEP", "v")]);
        assert_eq!(scrub_env(&e, &p), e);
    }

    #[test]
    fn strip_ld_preload_drops_both_loader_vars() {
        let p = pol(true, false, false, None);
        let e = env(&[
            ("LD_PRELOAD", "/evil.so"),
            ("LD_LIBRARY_PATH", "/evil/lib"),
            ("HOME", "/work"),
        ]);
        let out = scrub_env(&e, &p);
        assert_eq!(out, env(&[("HOME", "/work")]));
    }

    #[test]
    fn strip_path_replaces_with_fixed_default() {
        let p = pol(false, true, false, None);
        let e = env(&[("PATH", "/user/bin"), ("OTHER", "v")]);
        let out = scrub_env(&e, &p);
        assert_eq!(out, env(&[("OTHER", "v"), ("PATH", "/usr/bin:/bin")]));
    }

    #[test]
    fn tz_is_set_to_utc0_and_overrides_caller_supplied() {
        let p = pol(false, false, true, None);
        let e = env(&[("TZ", "America/New_York")]);
        let out = scrub_env(&e, &p);
        assert_eq!(out, env(&[("TZ", "UTC0")]));
    }

    #[test]
    fn source_date_epoch_injected_when_set() {
        let p = pol(false, false, false, Some(1_700_000_000));
        let out = scrub_env(&[], &p);
        assert_eq!(out, env(&[("SOURCE_DATE_EPOCH", "1700000000")]),);
    }
}
