//! `PeerObservability` returns node-local state and never fans out.
//!
//! The no-recursion guarantee for aggregation is structural rather than a
//! flag: this service has no code path that calls a peer. A flag can be
//! forgotten, mis-defaulted, or spoofed; a service with no recursion path
//! cannot be made to recurse.
//!
//! That property is "no such code path exists", which no runtime test can
//! demonstrate — so the test reads the source. It will fail loudly if a future
//! refactor adds one.

#![allow(clippy::unwrap_used, clippy::disallowed_methods, clippy::panic)]

const SRC: &str = include_str!("../src/services/peer_observability.rs");

#[test]
fn the_peer_service_never_calls_a_peer() {
    for forbidden in [
        "PeerObservabilityClient",
        "ObservabilityServiceClient",
        "ClusterSnapshot",
        "poll_peers",
        "PeerProbe",
    ] {
        assert!(
            !SRC.contains(forbidden),
            "peer_observability.rs references `{forbidden}`. This service must \
             return node-local state only: it is the structural guarantee that \
             aggregation cannot recurse, and a client or poller reference here \
             breaks it."
        );
    }
}

/// The file's brevity is part of the guarantee — a service that stays small is
/// a service you can still verify by reading. This is a smoke alarm, not a
/// style rule: if it fires, check what got added and why.
#[test]
fn the_peer_service_stays_small_enough_to_audit_by_reading() {
    let lines = SRC.lines().count();
    assert!(
        lines < 120,
        "peer_observability.rs is {lines} lines. It exists to do one thing; \
         if it has grown, check that it has not grown a fan-out path."
    );
}
