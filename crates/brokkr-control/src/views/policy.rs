//! Scheduling-policy projections (ADR 0014).

use std::collections::BTreeMap;

use crate::wasm_strategy::WasmStrategy;

/// Every failure reason `PolicyFailure::reason` can return.
///
/// Enumerated here so the view always reports every series, including zeroes.
/// A dashboard where a series appears the first time something breaks is a
/// dashboard that cannot show you "this has never happened".
pub const REASONS: &[&str] = &[
    "trap",
    "fuel_exhausted",
    "deadline",
    "bad_index",
    "instantiate",
    "memory",
    "not_loaded",
    "quarantined",
];

/// Scheduling-policy state, as an operator sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyView {
    /// Whether a WASM policy module is installed.
    pub loaded: bool,
    /// Whether the policy has been quarantined after repeated failures.
    pub quarantined: bool,
    /// Decisions the guest actually made.
    pub decided: u64,
    /// Decisions the guest declined, deferring to the built-in.
    pub declined: u64,
    /// Failures per reason tag. Every reason in [`REASONS`] is present.
    pub failures_by_reason: BTreeMap<String, u64>,
    /// The control-plane node this policy runs on.
    pub owning_node: String,
}

/// Project a node's scheduling policy into a [`PolicyView`].
///
/// `None` means no WASM policy is configured on this node — reported
/// explicitly rather than as an absent view, so an operator can distinguish
/// "no policy" from "policy is broken". Nodes may legitimately differ here,
/// which is why this carries `owning_node` like every other node-local DTO.
pub fn policy_view(strategy: Option<&WasmStrategy>, owning_node: &str) -> PolicyView {
    let Some(s) = strategy else {
        return PolicyView {
            loaded: false,
            quarantined: false,
            decided: 0,
            declined: 0,
            failures_by_reason: BTreeMap::new(),
            owning_node: owning_node.to_string(),
        };
    };
    let counts = s.failure_counts();
    PolicyView {
        loaded: true,
        quarantined: counts.for_reason("quarantined") > 0,
        decided: s.decided(),
        declined: s.declined(),
        failures_by_reason: REASONS
            .iter()
            .map(|r| ((*r).to_string(), counts.for_reason(r)))
            .collect(),
        owning_node: owning_node.to_string(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::disallowed_methods, clippy::panic)]
mod tests {
    use super::*;

    /// With no policy configured the view is explicit about it rather than
    /// absent, so an operator can tell "no policy" from "policy is broken".
    #[test]
    fn no_policy_configured_reports_not_loaded() {
        let v = policy_view(None, "node-1");
        assert!(!v.loaded);
        assert!(!v.quarantined);
        assert_eq!(v.decided, 0);
        assert_eq!(v.declined, 0);
        assert!(v.failures_by_reason.is_empty());
        assert_eq!(v.owning_node, "node-1");
    }

    /// Every reason tag is enumerated, so a loaded policy always reports every
    /// series including zeroes rather than having one appear the first time
    /// something breaks.
    #[test]
    fn all_failure_reasons_are_enumerated() {
        let expected = [
            "trap",
            "fuel_exhausted",
            "deadline",
            "bad_index",
            "instantiate",
            "memory",
            "not_loaded",
            "quarantined",
        ];
        assert_eq!(REASONS.len(), expected.len());
        for r in expected {
            assert!(REASONS.contains(&r), "missing reason tag {r}");
        }
    }

    /// A loaded policy reports every reason at zero before anything fails.
    #[test]
    fn a_loaded_policy_reports_every_reason_even_at_zero() {
        use brokkr_policy::{PolicyEngine, PolicyLimits};

        let engine = PolicyEngine::new(PolicyLimits::default()).unwrap();
        let strategy = WasmStrategy::new(engine, None);
        let v = policy_view(Some(&strategy), "node-1");

        assert!(v.loaded);
        assert!(!v.quarantined);
        assert_eq!(v.failures_by_reason.len(), REASONS.len());
        assert!(
            v.failures_by_reason.values().all(|c| *c == 0),
            "nothing has failed yet"
        );
    }
}
