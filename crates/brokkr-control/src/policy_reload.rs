//! Hot reload for the WASM scheduling policy (ADR 0014).
//!
//! An operator edits the `.wasm` file; the next decision uses the new module.
//! No restart, no RPC, no admin socket — the iteration loop the whole feature
//! exists to provide would be pointless if changing a policy meant bouncing
//! the control plane.
//!
//! The load-bearing invariant is **validate before swap**: a module that fails
//! to compile, is missing an export, speaks the wrong ABI version, or fails its
//! smoke decision never becomes the live policy. A bad edit costs the operator
//! a log line, not the scheduler. `PolicyEngine::load` enforces this, so this
//! module only has to decide *when* to call it.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use brokkr_common::Digest;

use tracing::Instrument as _;

use crate::wasm_strategy::WasmStrategy;

/// How often the policy file is checked for changes, by default.
///
/// A poll rather than an inotify watch: it is one fewer dependency, it works
/// the same across filesystems that do not report events (network mounts,
/// some container overlays), and it cannot miss an edit — the worst case is
/// noticing one interval late, which for a scheduling policy is nothing.
pub const DEFAULT_RELOAD_INTERVAL: Duration = Duration::from_secs(5);

/// Whether the observed content means the file should be reloaded.
///
/// Pure, so every transition is testable without touching a filesystem or
/// waiting on a timer — the same discipline as `rotation_plan` and
/// `resolve_raft_tls`.
///
/// A file that becomes *unreadable* (deleted, permissions changed) does **not**
/// trigger a reload: there is nothing to load, and the running policy is the
/// best thing available. The previous digest is deliberately kept in that case,
/// so restoring an identical file does not look like a change and cause a
/// needless recompile.
pub fn should_reload(last: Option<&Digest>, current: Option<&Digest>) -> bool {
    match (last, current) {
        // First successful read, or the content changed.
        (None, Some(_)) => true,
        (Some(prev), Some(now)) => prev != now,
        // Unreadable now: keep serving what we have.
        (_, None) => false,
    }
}

/// Read `path` and digest it, returning `None` if it cannot be read.
///
/// Content-addressed rather than `(mtime, len)`, and the difference is not
/// academic: a policy edit that swaps one constant for another changes neither
/// the length nor — within a filesystem's timestamp granularity, which is a
/// whole second on some — the mtime. A stat-based watcher silently ignores that
/// edit, which is precisely the "my change didn't take effect and nothing said
/// why" failure this feature must not have. This project is content-addressed
/// throughout; the policy file is no exception.
///
/// The cost is reading the file each interval. For a policy module every few
/// seconds that is nothing, and the bytes are needed anyway whenever it *has*
/// changed.
pub fn digest_of(path: &Path) -> Option<(Digest, Vec<u8>)> {
    let bytes = std::fs::read(path).ok()?;
    let digest = Digest::of(&bytes);
    Some((digest, bytes))
}

/// Watch `path` and reload `strategy` whenever it changes.
///
/// Runs until the task is aborted. Every outcome is logged, because a reload
/// that silently did not happen is exactly the failure mode that makes an
/// operator distrust the feature:
///
/// - reload succeeded → `info`, and any quarantine is cleared (reloading is the
///   documented fix path, so it must actually fix it).
/// - reload failed → `error` naming the reason, and **the running policy keeps
///   serving**. The stamp is still recorded, so a broken file is not recompiled
///   every interval; fixing it changes the stamp again and retries.
/// - file unreadable → `warn` once per transition, running policy untouched.
pub fn spawn_policy_reloader(
    strategy: Arc<WasmStrategy>,
    path: PathBuf,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    // Seed *synchronously*, before spawning: this must be the digest of what
    // the caller just loaded, not of whatever is on disk whenever the task
    // first happens to be polled. Doing it inside the task leaves a window in
    // which an edit lands first, gets absorbed into the seed, and is then never
    // seen as a change — the reload silently does nothing.
    let seed = digest_of(&path).map(|(d, _)| d);

    tokio::spawn(
        async move {
            if interval.is_zero() {
                tracing::info!(
                    policy = %path.display(),
                    "policy hot reload disabled (interval is zero)"
                );
                return;
            }
            let mut last = seed;
            let mut missing_logged = false;

            let mut ticker = tokio::time::interval(interval);
            ticker.tick().await;
            loop {
                ticker.tick().await;
                let Some((digest, bytes)) = digest_of(&path) else {
                    if !missing_logged {
                        tracing::warn!(
                            policy = %path.display(),
                            "scheduling policy file is unreadable; keeping the running policy"
                        );
                        missing_logged = true;
                    }
                    continue;
                };
                missing_logged = false;

                if !should_reload(last.as_ref(), Some(&digest)) {
                    continue;
                }
                // Record the digest before attempting the load, so a module
                // that fails to compile is not recompiled every interval.
                // Fixing it changes the digest again and retries.
                last = Some(digest);

                match strategy.load(&bytes) {
                    Ok(()) => tracing::info!(
                        policy = %path.display(),
                        bytes = bytes.len(),
                        "scheduling policy reloaded"
                    ),
                    Err(e) => tracing::error!(
                        policy = %path.display(),
                        error = %e,
                        "scheduling policy failed validation; the previously loaded \
                         policy is still serving"
                    ),
                }
            }
        }
        .in_current_span(),
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::disallowed_methods, clippy::panic)]
mod tests {
    use super::*;

    fn dig(bytes: &[u8]) -> Digest {
        Digest::of(bytes)
    }

    #[test]
    fn the_first_successful_read_triggers_a_reload() {
        assert!(should_reload(None, Some(&dig(b"module"))));
    }

    #[test]
    fn unchanged_content_does_not_reload() {
        let d = dig(b"module");
        assert!(!should_reload(Some(&d), Some(&d)));
    }

    /// The case a stat-based watcher misses: an edit that swaps one constant
    /// for another changes neither the length nor, within a filesystem's
    /// timestamp granularity, the mtime. Content addressing has no such blind
    /// spot — and this is not hypothetical, it is what the first version of
    /// this module actually got wrong.
    #[test]
    fn a_same_length_edit_reloads() {
        let before = dig(b"i32.const 0");
        let after = dig(b"i32.const 1");
        assert_eq!(
            b"i32.const 0".len(),
            b"i32.const 1".len(),
            "the fixture must be same-length or it proves nothing"
        );
        assert!(should_reload(Some(&before), Some(&after)));
    }

    #[test]
    fn a_different_length_edit_reloads() {
        assert!(should_reload(
            Some(&dig(b"short")),
            Some(&dig(b"much longer"))
        ));
    }

    /// A deleted or unreadable file must not reload — there is nothing to load,
    /// and the running policy is the best thing available.
    #[test]
    fn an_unreadable_file_never_triggers_a_reload() {
        assert!(!should_reload(Some(&dig(b"module")), None));
        assert!(!should_reload(None, None));
    }

    /// Restoring an identical file after it went missing must not look like a
    /// change, because the last digest is retained across the gap.
    #[test]
    fn restoring_identical_content_is_not_a_change() {
        let d = dig(b"module");
        assert!(!should_reload(Some(&d), None));
        assert!(!should_reload(Some(&d), Some(&d)));
    }

    /// Reverting to a previously seen version *is* a change and must reload —
    /// "roll back the policy" is a normal operator action.
    #[test]
    fn reverting_to_an_earlier_version_reloads() {
        let v1 = dig(b"policy v1");
        let v2 = dig(b"policy v2");
        assert!(should_reload(Some(&v1), Some(&v2)));
        assert!(should_reload(Some(&v2), Some(&v1)));
    }

    #[test]
    fn digest_of_a_missing_path_is_none() {
        assert!(digest_of(Path::new("/nonexistent/brokkr/policy.wasm")).is_none());
    }

    #[test]
    fn digest_of_returns_the_bytes_it_hashed() {
        let dir = std::env::temp_dir().join("brokkr-policy-digest-unit");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("p.bin");
        std::fs::write(&path, b"some policy bytes").unwrap();
        let (d, bytes) = digest_of(&path).unwrap();
        assert_eq!(bytes, b"some policy bytes");
        assert_eq!(d, Digest::of(&bytes));
        std::fs::remove_dir_all(&dir).ok();
    }
}
