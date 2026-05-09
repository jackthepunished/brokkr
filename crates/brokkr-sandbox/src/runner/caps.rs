//! Capability drop and `PR_SET_NO_NEW_PRIVS` for the runner (M7).
//!
//! See `docs/phase-2-plan.md` §5.7. The runner drops every capability
//! not explicitly retained via [`crate::SandboxConfig::retained_caps`]
//! and sets `no_new_privs` so subsequent `execve` cannot regain
//! privileges via setuid binaries or file capabilities.

use std::collections::HashSet;
use std::io;
use std::str::FromStr;

use caps::{CapSet, Capability};
use nix::libc;

/// Parse a list of textual capability names (e.g. `"CAP_NET_ADMIN"`) into
/// a `HashSet<Capability>`. Unknown names are reported as
/// [`io::ErrorKind::InvalidInput`].
#[allow(dead_code)] // wired up in M7-D
fn parse_retained(retained: &[String]) -> io::Result<HashSet<Capability>> {
    let mut out = HashSet::with_capacity(retained.len());
    for name in retained {
        let cap = Capability::from_str(name).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unknown capability: {name}"),
            )
        })?;
        out.insert(cap);
    }
    Ok(out)
}

/// Drop every capability not present in `retained` from all capability
/// sets (Bounding, Inheritable, Permitted, Effective, Ambient), then
/// set `PR_SET_NO_NEW_PRIVS=1`.
///
/// This is destructive to the calling thread/process and is intended
/// to run in the runner child immediately before `execve`.
#[allow(dead_code)] // wired up in M7-D
pub(super) fn drop_all_except(retained: &[String]) -> io::Result<()> {
    let keep = parse_retained(retained)?;

    // Bounding set: must be drained one cap at a time via PR_CAPBSET_DROP.
    // `caps::set` does not support the bounding set.
    let bounding = caps::read(None, CapSet::Bounding)
        .map_err(|e| io::Error::other(format!("caps {:?}: {e}", CapSet::Bounding)))?;
    for cap in bounding.iter().copied() {
        if !keep.contains(&cap) {
            caps::drop(None, CapSet::Bounding, cap)
                .map_err(|e| io::Error::other(format!("caps {:?}: {e}", CapSet::Bounding)))?;
        }
    }

    // Inheritable / Permitted / Effective: replace the set with the
    // intersection of the current set and `keep`.
    for set in [CapSet::Inheritable, CapSet::Permitted, CapSet::Effective] {
        let current =
            caps::read(None, set).map_err(|e| io::Error::other(format!("caps {set:?}: {e}")))?;
        let new: HashSet<Capability> = current.intersection(&keep).copied().collect();
        if new != current {
            caps::set(None, set, &new)
                .map_err(|e| io::Error::other(format!("caps {set:?}: {e}")))?;
        }
    }

    // Ambient set: kernel >= 4.3. If unsupported, log and continue.
    match caps::read(None, CapSet::Ambient) {
        Ok(current) => {
            let new: HashSet<Capability> = current.intersection(&keep).copied().collect();
            if new != current {
                caps::set(None, CapSet::Ambient, &new)
                    .map_err(|e| io::Error::other(format!("caps {:?}: {e}", CapSet::Ambient)))?;
            }
        }
        Err(e) => {
            tracing::debug!(error = %e, "ambient capability set unsupported; skipping");
        }
    }

    // PR_SET_NO_NEW_PRIVS. nix 0.29 does not expose a typed wrapper for
    // this prctl variant, so we call libc directly.
    //
    // SAFETY: `prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0)` reads no pointers,
    // takes only scalar arguments, and is documented as async-signal-safe.
    // It cannot violate Rust memory safety; failure is reported via the
    // return value and `errno`.
    #[allow(unsafe_code)]
    let rc = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }

    tracing::debug!(
        retained = retained.len(),
        "dropped capabilities; PR_SET_NO_NEW_PRIVS=1"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::expect_used)]
    fn parses_known_capability_names() {
        let parsed = parse_retained(&["CAP_NET_ADMIN".to_string(), "CAP_SYS_ADMIN".to_string()])
            .expect("known cap names should parse");
        assert_eq!(parsed.len(), 2);
        assert!(parsed.contains(&Capability::CAP_NET_ADMIN));
        assert!(parsed.contains(&Capability::CAP_SYS_ADMIN));
    }

    #[test]
    fn rejects_unknown_capability() {
        let result = parse_retained(&["CAP_NOPE".to_string()]);
        assert!(result.is_err());
        if let Err(e) = result {
            assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
        }
    }
}
