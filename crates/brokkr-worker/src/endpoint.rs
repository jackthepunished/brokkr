//! Control-plane endpoint set + reconnect rotation policy (Phase 5 I9b W4).
//!
//! A worker pinned to one control-plane address dies with that node, which
//! defeats the point of an HA control plane: the cluster survives a leader
//! kill but the worker that was talking to the dead node never comes back
//! (`docs/phase-5-plan.md` §VII.1 gap 3).
//!
//! The policy here is deliberately a **pure function** of (endpoint count,
//! attempt number). Reconnect logic that is tangled into the I/O path can only
//! be tested by killing real servers; this way the interesting behaviour —
//! which endpoint next, how long to wait — is exhaustively unit-testable, and
//! `worker::run_worker` keeps only the part that genuinely needs a socket.

use std::time::Duration;

/// One control-plane node, as the worker addresses it.
///
/// Two URLs because the control plane may split its listeners (issue #139):
/// the worker port carries `WorkerService` **and** CAS for mTLS-authenticated
/// workers, while the client port is JWT-gated and unusable by a worker. When
/// [`Self::worker`] is `None` the node runs single-port and [`Self::client`]
/// serves both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlPlane {
    /// The client-facing gRPC URL (CAS / ActionCache / Capabilities /
    /// Execution).
    pub client: String,
    /// The worker-facing gRPC URL (`WorkerService` + CAS), when the node
    /// splits its listeners.
    pub worker: Option<String>,
}

impl ControlPlane {
    /// A single-port node: one URL serving both planes.
    pub fn single_port(url: impl Into<String>) -> Self {
        Self {
            client: url.into(),
            worker: None,
        }
    }

    /// The URL to use for `WorkerService` and CAS: the worker port when the
    /// node splits listeners, else the client port.
    pub fn worker_url(&self) -> &str {
        self.worker.as_deref().unwrap_or(&self.client)
    }
}

/// Base wait after a full cycle through every endpoint has failed.
///
/// Public because it is part of the reconnect contract an operator reasons
/// about (and because [`rotation_plan`]'s documentation refers to it).
pub const BASE_BACKOFF: Duration = Duration::from_millis(250);

/// Ceiling on the reconnect wait, so a long outage settles into steady polling
/// instead of growing without bound.
///
/// Public for the same reason as [`BASE_BACKOFF`]: these two values *are* the
/// policy, and a reader of [`rotation_plan`] needs to see them.
pub const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Which endpoint to try for `attempt`, and how long to wait first.
///
/// `attempt` counts from 0 and never resets while the worker is failing, so the
/// policy has two phases that matter:
///
/// * **Within the first cycle** (`attempt < len`) every endpoint is tried with
///   **no delay**. A leader kill should cost a worker milliseconds, not
///   seconds — the surviving nodes are up *now*, and backing off before having
///   tried them would add latency for no reason.
/// * **After a full cycle** the whole cluster looks unreachable, so the wait
///   doubles per completed cycle up to [`MAX_BACKOFF`]. That is the case where
///   patience is correct: nothing is listening and hammering helps nobody.
///
/// The jitter is derived from `attempt` rather than an RNG: a fleet of workers
/// restarted together must not reconnect in lockstep (the same reasoning that
/// makes I9a derive election seeds from the node id), and a deterministic
/// policy stays unit-testable. Per-worker de-synchronisation comes from
/// workers failing at different attempt counts and wall-clock offsets.
pub fn rotation_plan(len: usize, attempt: usize) -> (usize, Duration) {
    if len == 0 {
        return (0, MAX_BACKOFF);
    }
    let index = attempt % len;
    let completed_cycles = attempt / len;
    if completed_cycles == 0 {
        return (index, Duration::ZERO);
    }
    // Saturating shift: after ~7 cycles this is pinned at MAX_BACKOFF anyway,
    // and `1u64 << 64` would be undefined-shift panic territory.
    let factor = 1u64
        .checked_shl(completed_cycles as u32 - 1)
        .unwrap_or(u64::MAX);
    let millis = BASE_BACKOFF
        .as_millis()
        .saturating_mul(u128::from(factor))
        .min(MAX_BACKOFF.as_millis());
    // Deterministic spread of up to 100 ms so co-restarted workers separate.
    let jitter = (attempt as u128 * 37) % 100;
    let millis = (millis + jitter).min(MAX_BACKOFF.as_millis()) as u64;
    (index, Duration::from_millis(millis))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::disallowed_methods)]
mod tests {
    use super::*;

    #[test]
    fn worker_url_prefers_the_worker_port_and_falls_back_to_the_client_port() {
        let single = ControlPlane::single_port("http://a:7878");
        assert_eq!(single.worker_url(), "http://a:7878");

        let split = ControlPlane {
            client: "https://a:7878".to_string(),
            worker: Some("https://a:7879".to_string()),
        };
        assert_eq!(split.worker_url(), "https://a:7879");
    }

    #[test]
    fn the_first_cycle_tries_every_endpoint_immediately() {
        // A leader kill must cost milliseconds: all three are tried with no
        // delay before any backoff is introduced.
        for attempt in 0..3 {
            let (index, wait) = rotation_plan(3, attempt);
            assert_eq!(index, attempt, "endpoints are tried in order");
            assert_eq!(wait, Duration::ZERO, "no delay within the first cycle");
        }
    }

    #[test]
    fn backoff_grows_only_after_a_full_cycle_and_is_capped() {
        // Second cycle: the cluster looked unreachable once, so start waiting.
        let (index, wait) = rotation_plan(3, 3);
        assert_eq!(index, 0, "rotation wraps to the first endpoint");
        assert!(wait >= BASE_BACKOFF, "backoff begins after one full cycle");

        // Growth is monotonic across cycles at the same index...
        let mut previous = Duration::ZERO;
        for cycle in 1..8 {
            let (_, wait) = rotation_plan(3, cycle * 3);
            assert!(
                wait >= previous,
                "cycle {cycle}: backoff must not shrink ({wait:?} < {previous:?})"
            );
            previous = wait;
        }

        // ...and never exceeds the ceiling, including far past the point where
        // a naive `1 << n` would overflow.
        for attempt in [30usize, 300, 3_000, usize::MAX / 2] {
            let (_, wait) = rotation_plan(3, attempt);
            assert!(wait <= MAX_BACKOFF, "attempt {attempt} exceeded the cap");
        }
    }

    #[test]
    fn a_single_endpoint_still_rotates_onto_itself_with_backoff() {
        // The common single-node case: there is nowhere else to go, so the
        // policy degrades to "retry the one endpoint, patiently".
        assert_eq!(rotation_plan(1, 0), (0, Duration::ZERO));
        let (index, wait) = rotation_plan(1, 1);
        assert_eq!(index, 0);
        assert!(wait >= BASE_BACKOFF);
    }

    #[test]
    fn an_empty_endpoint_set_cannot_panic() {
        // Defensive: config validation rejects this, but a modulo by zero here
        // would take the worker down rather than report a bad flag.
        let (index, wait) = rotation_plan(0, 5);
        assert_eq!(index, 0);
        assert_eq!(wait, MAX_BACKOFF);
    }

    #[test]
    fn jitter_separates_co_restarted_workers_without_unbounded_growth() {
        // Two workers at different attempt counts in the same cycle must not
        // wake at exactly the same moment.
        let (_, a) = rotation_plan(3, 3);
        let (_, b) = rotation_plan(3, 4);
        assert_ne!(a, b, "attempts within a cycle must not collide exactly");
        assert!(a <= MAX_BACKOFF && b <= MAX_BACKOFF);
    }
}
