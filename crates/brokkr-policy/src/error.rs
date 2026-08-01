//! Failure taxonomy for the scheduling-policy engine (ADR 0014).
//!
//! Two types, because two things fail for different reasons and want different
//! handling:
//!
//! - [`PolicyError`] — **loading** a module failed. Loud. The operator gave the
//!   control plane something unusable, and the running policy (if any) is left
//!   untouched.
//! - [`PolicyFailure`] — a **decision** failed. Quiet-ish: warn, count, and
//!   fall back to the built-in for that one placement. A broken policy must not
//!   become a broken cluster.

use thiserror::Error;

/// A module could not be loaded.
///
/// Every variant means "the running policy, if any, is unchanged" — validation
/// happens before any swap, so a bad edit costs the operator a log line rather
/// than the cluster its scheduler.
#[derive(Debug, Error)]
pub enum PolicyError {
    /// The bytes are not a valid WebAssembly module, or failed to compile.
    #[error("compiling the policy module: {0}")]
    Compile(String),

    /// A required export is missing or has the wrong signature.
    ///
    /// Caught at load rather than on the first decision, so an operator finds
    /// out when they deploy the module and not at 3am under load.
    #[error("policy module is missing export `{name}` (or its signature is wrong): {detail}")]
    MissingExport {
        /// The export that was required.
        name: &'static str,
        /// What wasmtime said.
        detail: String,
    },

    /// The module was compiled against a different ABI version.
    ///
    /// The one load failure that could plausibly have been a runtime
    /// degradation instead, and deliberately isn't: a version-mismatched module
    /// would misparse every snapshot *silently* and return plausible-looking
    /// wrong placements. A refusal is far better than confident nonsense.
    #[error("policy module speaks ABI version {found}, this control plane speaks {expected}")]
    AbiMismatch {
        /// What the module reported.
        found: u32,
        /// What this engine speaks.
        expected: u32,
    },

    /// The module compiled and exported the right things, but failed the
    /// synthetic decision run at load time.
    ///
    /// This is what stops a module that traps on the very first call from ever
    /// becoming the live policy.
    #[error("policy module failed its load-time smoke decision: {0}")]
    SmokeTest(PolicyFailure),

    /// The engine itself could not be configured (a wasmtime setup error).
    #[error("configuring the policy engine: {0}")]
    Engine(String),

    /// The crate was built without the `wasm-policy` feature.
    #[error("this build has no WASM policy support (the `wasm-policy` feature is off)")]
    FeatureDisabled,
}

/// One decision failed.
///
/// Every variant takes the same path at the call site: `warn!` with the reason,
/// increment the per-reason counter, and use the built-in policy's answer for
/// that placement. The variants exist so an operator can tell *which* thing is
/// wrong from the counter alone — "your policy is too slow" and "your policy
/// returns garbage indices" need different fixes.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PolicyFailure {
    /// The guest trapped — `unreachable`, an out-of-bounds access, a division
    /// by zero, an unwrapped panic in a Rust-compiled policy.
    #[error("policy trapped: {0}")]
    Trap(String),

    /// The guest ran out of fuel: it did too much *work* for one decision.
    #[error("policy exhausted its fuel budget")]
    FuelExhausted,

    /// The guest exceeded its wall-clock deadline (epoch interruption).
    ///
    /// Distinct from [`Self::FuelExhausted`] because the fixes differ: fuel
    /// exhaustion means the policy is doing too much, whereas a deadline hit on
    /// modest fuel usually means the host is oversubscribed.
    #[error("policy exceeded its wall-clock deadline")]
    Deadline,

    /// The guest returned an index that is neither [`crate::DECLINE`] nor a
    /// valid position in the candidate list.
    #[error(
        "policy returned index {returned}, which is not a candidate (0..{candidates}) or DECLINE"
    )]
    BadIndex {
        /// What the guest returned.
        returned: i32,
        /// How many candidates it was given.
        candidates: usize,
    },

    /// The module could not be instantiated for this decision.
    #[error("instantiating the policy for this decision: {0}")]
    Instantiate(String),

    /// The guest's allocator returned a pointer the snapshot does not fit at,
    /// or its memory export is missing at call time.
    #[error("policy could not receive the snapshot: {0}")]
    Memory(String),

    /// No policy is loaded. Not really a failure of the policy, but it reaches
    /// the same fallback, and counting it separately distinguishes "misconfigured"
    /// from "broken".
    #[error("no policy module is loaded")]
    NotLoaded,

    /// The policy has been quarantined after too many consecutive failures and
    /// is no longer being called.
    #[error("policy is quarantined after {consecutive} consecutive failures; reload the module to clear it")]
    Quarantined {
        /// How many consecutive failures tripped the quarantine.
        consecutive: u32,
    },
}

impl PolicyFailure {
    /// A short, stable, lowercase tag for this failure, for use as a metric
    /// label or log field.
    ///
    /// Stable across releases — an operator's dashboard groups on it — and
    /// deliberately free of any interpolated detail so the label cardinality
    /// stays bounded.
    pub fn reason(&self) -> &'static str {
        match self {
            Self::Trap(_) => "trap",
            Self::FuelExhausted => "fuel_exhausted",
            Self::Deadline => "deadline",
            Self::BadIndex { .. } => "bad_index",
            Self::Instantiate(_) => "instantiate",
            Self::Memory(_) => "memory",
            Self::NotLoaded => "not_loaded",
            Self::Quarantined { .. } => "quarantined",
        }
    }

    /// Whether this failure should count toward the quarantine threshold.
    ///
    /// `NotLoaded` and `Quarantined` do not: neither is evidence about the
    /// *module*, and counting `Quarantined` would make the counter climb
    /// forever once tripped, turning a diagnostic into noise.
    pub fn counts_toward_quarantine(&self) -> bool {
        !matches!(self, Self::NotLoaded | Self::Quarantined { .. })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::disallowed_methods, clippy::panic)]
mod tests {
    use super::*;

    /// Every variant must have a distinct reason tag, or two different problems
    /// become indistinguishable on a dashboard.
    #[test]
    fn reason_tags_are_distinct() {
        let all = [
            PolicyFailure::Trap("x".into()),
            PolicyFailure::FuelExhausted,
            PolicyFailure::Deadline,
            PolicyFailure::BadIndex {
                returned: 9,
                candidates: 2,
            },
            PolicyFailure::Instantiate("x".into()),
            PolicyFailure::Memory("x".into()),
            PolicyFailure::NotLoaded,
            PolicyFailure::Quarantined { consecutive: 16 },
        ];
        let mut tags: Vec<&str> = all.iter().map(|f| f.reason()).collect();
        let count = tags.len();
        tags.sort_unstable();
        tags.dedup();
        assert_eq!(tags.len(), count, "reason tags must be unique");
    }

    /// Reason tags carry no interpolated detail, so metric label cardinality
    /// stays bounded no matter what a guest does.
    #[test]
    fn reason_tags_are_free_of_detail() {
        let f = PolicyFailure::Trap("wasm trap: unreachable at 0xdeadbeef".into());
        assert_eq!(f.reason(), "trap");
        let f = PolicyFailure::BadIndex {
            returned: 99999,
            candidates: 2,
        };
        assert_eq!(f.reason(), "bad_index");
    }

    #[test]
    fn only_module_failures_count_toward_quarantine() {
        assert!(PolicyFailure::Trap("x".into()).counts_toward_quarantine());
        assert!(PolicyFailure::FuelExhausted.counts_toward_quarantine());
        assert!(PolicyFailure::Deadline.counts_toward_quarantine());
        assert!(PolicyFailure::BadIndex {
            returned: 4,
            candidates: 1
        }
        .counts_toward_quarantine());
        assert!(PolicyFailure::Memory("x".into()).counts_toward_quarantine());
        assert!(PolicyFailure::Instantiate("x".into()).counts_toward_quarantine());

        // Not evidence about the module.
        assert!(!PolicyFailure::NotLoaded.counts_toward_quarantine());
        // Would otherwise climb forever once tripped.
        assert!(!PolicyFailure::Quarantined { consecutive: 16 }.counts_toward_quarantine());
    }

    /// The messages are what an operator reads at 3am; they must name the
    /// thing to fix.
    #[test]
    fn messages_name_the_actionable_detail() {
        let f = PolicyFailure::BadIndex {
            returned: 7,
            candidates: 3,
        };
        let msg = f.to_string();
        assert!(msg.contains('7') && msg.contains('3'), "got: {msg}");

        let e = PolicyError::AbiMismatch {
            found: 2,
            expected: 1,
        };
        let msg = e.to_string();
        assert!(msg.contains('2') && msg.contains('1'), "got: {msg}");

        let q = PolicyFailure::Quarantined { consecutive: 16 };
        assert!(q.to_string().contains("reload"), "must say how to clear it");
    }
}
