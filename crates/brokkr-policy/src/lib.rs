//! WebAssembly scheduling-policy engine for Brokkr (ADR 0014).
//!
//! An operator points the control plane at a `.wasm` module; this crate loads
//! it, validates it, and runs one bounded call per placement decision. It
//! knows nothing about `Strategy`, workers-as-such, or the scheduler — it takes
//! an encoded `brokkr.v1.DecisionSnapshot` and returns a candidate index.
//! `brokkr-control` owns the adapter that connects the two.
//!
//! That direction matters: putting the `Strategy` trait here would require this
//! crate to depend on `brokkr-control`, which depends on this crate. The crate
//! graph is a DAG and stays one.
//!
//! # Why wasmtime
//!
//! `Strategy::choose` is called synchronously while the scheduler's dispatch
//! mutex is held, so a guest that stalls stalls placement for the whole
//! cluster. Fuel bounds *work*; only epoch interruption bounds *wall-clock
//! time*. wasmtime is the only Rust WASM runtime that offers it, and given the
//! failure posture is "degrade rather than fail", a hard time bound is what
//! makes that posture honest instead of aspirational.
//!
//! # Why a fresh `Store` per call
//!
//! The scheduler's `Strategy` is `Send + Sync`; `wasmtime::Store` is `Send` but
//! not `Sync`. Rather than fight that, no `Store` is ever held: the engine
//! keeps only an `Engine` and an `InstancePre` (both `Send + Sync`) and builds
//! a `Store` per decision. Reusing one would carry the previous decision's
//! guest heap into the next, so the same snapshot could yield different answers
//! depending on call history — destroying the determinism the whole test
//! strategy rests on, to save what the pooling allocator already provides.

#![deny(missing_docs)]

mod error;

#[cfg(feature = "wasm-policy")]
mod engine;

pub use error::{PolicyError, PolicyFailure};

#[cfg(feature = "wasm-policy")]
pub use engine::PolicyEngine;

/// Default fuel budget for one decision. Bounds *work*, catching an accidental
/// O(n²) in a policy long before the deadline would.
pub const DEFAULT_FUEL: u64 = 1_000_000;

/// Default wall-clock budget for one decision, in milliseconds. Bounds *time*,
/// which fuel cannot: this is the budget that actually protects the dispatch
/// mutex.
pub const DEFAULT_DEADLINE_MS: u64 = 5;

/// Consecutive failures before a policy is quarantined.
///
/// Falling back per-decision is correct but not sufficient on its own: a policy
/// that traps on every call would burn its full deadline forever while
/// dutifully logging about it. After this many consecutive failures the engine
/// stops calling the guest entirely and reports [`PolicyFailure::Quarantined`],
/// until the module is reloaded.
pub const QUARANTINE_THRESHOLD: u32 = 16;

/// The guest export returning the ABI version it was compiled against.
pub const EXPORT_ABI_VERSION: &str = "brokkr_abi_version";
/// The guest export allocating a buffer for the host to write the snapshot to.
pub const EXPORT_ALLOC: &str = "brokkr_alloc";
/// The guest export making the decision.
pub const EXPORT_CHOOSE: &str = "brokkr_choose";
/// The guest's exported linear memory.
pub const EXPORT_MEMORY: &str = "memory";

/// The index a guest returns to decline.
///
/// Mirrors `brokkr_control::policy_abi::DECLINE`, duplicated rather than shared
/// because this crate must not depend on `brokkr-control` (see the module
/// docs). A test in each crate pins the literal value.
pub const DECLINE: i32 = -1;

/// ABI version this engine speaks.
///
/// A module reporting anything else is refused at load — the one failure that
/// is loud rather than degrading, because a version-mismatched module would
/// otherwise misparse every snapshot silently and produce plausible-looking
/// wrong placements.
pub const POLICY_ABI_VERSION: u32 = 1;

/// What a guest returned, once validated against the candidate count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// The guest chose this candidate index. Always in range.
    Chose(usize),
    /// The guest returned [`DECLINE`], meaning "no preference — use the
    /// built-in". Not a failure, and does not count toward quarantine: a
    /// policy that does not recognise a job should be able to say so rather
    /// than guess.
    Declined,
}

/// Tunables for one [`PolicyEngine`].
#[derive(Debug, Clone, Copy)]
pub struct PolicyLimits {
    /// Fuel granted to each decision.
    pub fuel: u64,
    /// Wall-clock budget for each decision, in milliseconds.
    pub deadline_ms: u64,
    /// Consecutive failures before quarantine.
    pub quarantine_threshold: u32,
}

impl Default for PolicyLimits {
    fn default() -> Self {
        Self {
            fuel: DEFAULT_FUEL,
            deadline_ms: DEFAULT_DEADLINE_MS,
            quarantine_threshold: QUARANTINE_THRESHOLD,
        }
    }
}

/// Interpret a guest's raw return value against the candidate count.
///
/// Pure, so the entire return-value contract is testable without a runtime.
///
/// - [`DECLINE`] is [`Decision::Declined`].
/// - `0..candidate_count` is [`Decision::Chose`].
/// - Anything else — negative but not `DECLINE`, or past the end — is
///   [`PolicyFailure::BadIndex`]. A guest that miscounts must not be allowed to
///   silently place work on candidate 0.
pub fn interpret(raw: i32, candidate_count: usize) -> Result<Decision, PolicyFailure> {
    if raw == DECLINE {
        return Ok(Decision::Declined);
    }
    let bad = || PolicyFailure::BadIndex {
        returned: raw,
        candidates: candidate_count,
    };
    let idx = usize::try_from(raw).map_err(|_| bad())?;
    if idx < candidate_count {
        Ok(Decision::Chose(idx))
    } else {
        Err(bad())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::disallowed_methods, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn a_valid_index_is_a_choice() {
        assert_eq!(interpret(0, 3).unwrap(), Decision::Chose(0));
        assert_eq!(interpret(2, 3).unwrap(), Decision::Chose(2));
    }

    #[test]
    fn decline_is_not_a_failure() {
        assert_eq!(interpret(DECLINE, 3).unwrap(), Decision::Declined);
        // Declining with no candidates is still a decline, not a bad index.
        assert_eq!(interpret(DECLINE, 0).unwrap(), Decision::Declined);
    }

    #[test]
    fn an_out_of_range_index_is_a_failure_not_a_silent_clamp() {
        // Past the end.
        assert!(matches!(
            interpret(3, 3),
            Err(PolicyFailure::BadIndex {
                returned: 3,
                candidates: 3
            })
        ));
        // Negative but not the sentinel: -2 must not be mistaken for DECLINE.
        assert!(matches!(
            interpret(-2, 3),
            Err(PolicyFailure::BadIndex { .. })
        ));
        assert!(matches!(
            interpret(i32::MIN, 3),
            Err(PolicyFailure::BadIndex { .. })
        ));
        // Any index at all is out of range when there are no candidates.
        assert!(matches!(
            interpret(0, 0),
            Err(PolicyFailure::BadIndex { .. })
        ));
    }

    #[test]
    fn the_abi_constants_match_the_control_plane() {
        // Duplicated across the crate boundary on purpose (the DAG forbids the
        // dependency), so pin the literal values rather than trusting the two
        // definitions to be edited together.
        assert_eq!(DECLINE, -1);
        assert_eq!(POLICY_ABI_VERSION, 1);
    }
}
