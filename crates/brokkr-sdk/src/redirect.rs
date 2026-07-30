//! Leader-redirect policy for talking to an HA control plane (Phase 5 I9b W3).
//!
//! Metadata writes are served only by the Raft leader; a follower refuses with
//! gRPC `FAILED_PRECONDITION` and names the leader in metadata (I9b W1/W2). A
//! client that ignores that hint sees a hard failure against a perfectly
//! healthy cluster, which is the gap `docs/phase-5-plan.md` §VII.1 lists as
//! gap 2.
//!
//! The decision — *is this a redirect, and where to* — is a pure function of
//! the `Status` and the hop count, so every branch is unit-testable without a
//! cluster. Only the reconnect itself needs a socket.

use tonic::{Code, Status};

/// Metadata key carrying the leader's node id (server side: I8c).
pub const LEADER_HINT_METADATA_KEY: &str = "x-brokkr-leader";

/// Metadata key carrying the leader's dialable address (server side: I9b).
pub const LEADER_ADDR_METADATA_KEY: &str = "x-brokkr-leader-addr";

/// How many redirects to follow before giving up.
///
/// Three is enough for any real topology — a correct cluster redirects once —
/// while still terminating promptly when a stale-hint cycle forms (node A
/// names B, B names A). Following redirects without a bound turns a
/// mid-election cluster into an infinite loop in the client.
pub const MAX_LEADER_HOPS: usize = 3;

/// What a client should do with a failed RPC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Redirect {
    /// Not a leader redirect — the caller should surface the error as-is.
    None,
    /// A redirect naming a dialable address: reconnect there and retry.
    Follow {
        /// The leader's address as advertised (`host:port`).
        addr: String,
        /// The leader's node id, when present (for logging).
        leader: Option<String>,
    },
    /// A redirect that cannot be followed: the leader is named but has not
    /// published an address yet (the window between an election and its
    /// `cfg/nodes/<id>` record committing), or no leader is known at all.
    /// The caller should retry its *configured* endpoints rather than give up
    /// — the cluster is likely mid-election.
    Unroutable {
        /// The leader's node id, when the server knew it.
        leader: Option<String>,
    },
    /// The hop budget is spent. Something is wrong with the cluster's idea of
    /// its own leadership; report it instead of looping.
    Exhausted,
}

/// Classify a failed RPC. `hops` is how many redirects have already been
/// followed for this logical operation.
pub fn classify(status: &Status, hops: usize) -> Redirect {
    // Only a leadership refusal is a redirect. Reusing FAILED_PRECONDITION for
    // this is the server's existing contract (I8c), so the metadata — not the
    // code alone — is what identifies it: another component could legitimately
    // return FAILED_PRECONDITION for its own reasons.
    if status.code() != Code::FailedPrecondition {
        return Redirect::None;
    }
    let leader = metadata_str(status, LEADER_HINT_METADATA_KEY);
    let addr = metadata_str(status, LEADER_ADDR_METADATA_KEY);
    if leader.is_none() && addr.is_none() {
        return Redirect::None;
    }
    if hops >= MAX_LEADER_HOPS {
        return Redirect::Exhausted;
    }
    match addr {
        Some(addr) => Redirect::Follow { addr, leader },
        None => Redirect::Unroutable { leader },
    }
}

/// Read a metadata value as a `String`, ignoring non-ASCII values.
fn metadata_str(status: &Status, key: &str) -> Option<String> {
    status
        .metadata()
        .get(key)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

/// Turn a leader hint into a URL to dial, inheriting the scheme from the
/// endpoint we were already talking to.
///
/// The server advertises `host:port` — a *location*, not a URL — because it
/// does not know how clients reach it (plain vs TLS is the client's
/// configuration). Inheriting the current scheme keeps a TLS client on TLS: a
/// redirect must never silently downgrade an https session to http, which is
/// exactly the kind of thing a naive `format!("http://{addr}")` would do.
pub fn hint_to_url(addr: &str, current_endpoint: &str) -> String {
    if addr.contains("://") {
        return addr.to_string();
    }
    let scheme = current_endpoint.split_once("://").map(|(s, _)| s);
    match scheme {
        Some(scheme) => format!("{scheme}://{addr}"),
        // No scheme to inherit: assume plaintext, matching the SDK's other
        // default, and let the connect attempt report the truth.
        None => format!("http://{addr}"),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::disallowed_methods)]
mod tests {
    use super::*;

    fn redirect_status(leader: Option<&str>, addr: Option<&str>) -> Status {
        let mut status = Status::failed_precondition("not the metadata leader");
        if let Some(leader) = leader {
            status
                .metadata_mut()
                .insert(LEADER_HINT_METADATA_KEY, leader.parse().unwrap());
        }
        if let Some(addr) = addr {
            status
                .metadata_mut()
                .insert(LEADER_ADDR_METADATA_KEY, addr.parse().unwrap());
        }
        status
    }

    #[test]
    fn a_hinted_refusal_is_followed() {
        let status = redirect_status(Some("control-1"), Some("10.0.0.1:7878"));
        assert_eq!(
            classify(&status, 0),
            Redirect::Follow {
                addr: "10.0.0.1:7878".to_string(),
                leader: Some("control-1".to_string()),
            }
        );
    }

    #[test]
    fn an_id_without_an_address_is_unroutable_not_fatal() {
        // The election window: the leader is known but has not published its
        // address yet. Giving up here would fail a build against a healthy
        // cluster for a few milliseconds' worth of replication lag.
        let status = redirect_status(Some("control-2"), None);
        assert_eq!(
            classify(&status, 0),
            Redirect::Unroutable {
                leader: Some("control-2".to_string())
            }
        );
    }

    #[test]
    fn unrelated_failures_are_not_redirects() {
        // Other codes are never redirects...
        for status in [
            Status::internal("boom"),
            Status::not_found("no cached action result"),
            Status::deadline_exceeded("slow"),
            Status::unauthenticated("no token"),
        ] {
            assert_eq!(classify(&status, 0), Redirect::None, "{status:?}");
        }
        // ...and FAILED_PRECONDITION *without* leader metadata belongs to
        // whoever else uses that code (the scheduler returns it for "no
        // eligible worker"), so it must pass through untouched rather than
        // being retried as a redirect.
        assert_eq!(
            classify(&Status::failed_precondition("no eligible worker"), 0),
            Redirect::None
        );
    }

    #[test]
    fn the_hop_budget_terminates_a_redirect_cycle() {
        // A stale-hint cycle (A names B, B names A) must stop, and must stop
        // as Exhausted rather than as a successful-looking None.
        let status = redirect_status(Some("control-1"), Some("10.0.0.1:7878"));
        for hops in 0..MAX_LEADER_HOPS {
            assert!(matches!(classify(&status, hops), Redirect::Follow { .. }));
        }
        assert_eq!(classify(&status, MAX_LEADER_HOPS), Redirect::Exhausted);
        assert_eq!(
            classify(&status, MAX_LEADER_HOPS + 100),
            Redirect::Exhausted
        );
    }

    #[test]
    fn an_empty_metadata_value_is_treated_as_absent() {
        let status = redirect_status(Some(""), Some(""));
        assert_eq!(classify(&status, 0), Redirect::None);
    }

    #[test]
    fn a_redirect_never_downgrades_the_scheme() {
        // The load-bearing case: an mTLS client redirected by a follower must
        // stay on https, or the retry fails the handshake (or worse, succeeds
        // in plaintext).
        assert_eq!(
            hint_to_url("10.0.0.1:7878", "https://control-0:7878"),
            "https://10.0.0.1:7878"
        );
        assert_eq!(
            hint_to_url("10.0.0.1:7878", "http://control-0:7878"),
            "http://10.0.0.1:7878"
        );
        // A hint that already carries a scheme is authoritative.
        assert_eq!(
            hint_to_url("https://a:1", "http://b:2"),
            "https://a:1".to_string()
        );
        // A malformed current endpoint still yields something dialable.
        assert_eq!(hint_to_url("a:1", "garbage"), "http://a:1".to_string());
    }
}
