//! Phase 1 definition-of-done assertions (plan §13):
//!  1. The end-to-end test passes deterministically 100 times in a row.
//!  2. Cache hit is measurably faster than cache miss.

#![allow(clippy::unwrap_used, clippy::panic, clippy::disallowed_methods)]

use std::time::Instant;

use brokkr_sdk::{run_command, BrokkrClient};

mod common;
use common::boot_cluster;

/// Run 100 distinct echo commands, twice each, against a single cluster and
/// assert every first run is a cache miss and every second run is a cache
/// hit. 200 RPCs total.
///
/// Marked `#[ignore]` because it adds ~seconds to the suite. Run locally with
/// `cargo test -p brokkr-control --test phase1_dod -- --ignored`.
#[tokio::test]
#[ignore = "Phase 1 DoD soak; run with --ignored"]
async fn one_hundred_iterations_deterministic() {
    let (endpoint, _dir) = boot_cluster().await;
    let mut client = BrokkrClient::connect(endpoint).await.unwrap();

    for i in 0..100 {
        let argv = vec!["/bin/echo".to_string(), format!("phase1-soak-{i}")];

        let first = run_command(&mut client, &argv, false)
            .await
            .unwrap_or_else(|e| panic!("iter {i} miss: {e}"));
        assert_eq!(first.exit_code, 0, "iter {i} miss exit");
        assert!(!first.cache_hit, "iter {i} unexpected hit on first run");

        let second = run_command(&mut client, &argv, false)
            .await
            .unwrap_or_else(|e| panic!("iter {i} hit: {e}"));
        assert_eq!(second.exit_code, 0, "iter {i} hit exit");
        assert!(second.cache_hit, "iter {i} expected hit, got miss");
    }
}

/// Take 10 (miss, hit) pairs over distinct echo commands and assert the
/// median hit is faster than the median miss. The plan asks only for
/// "measurably faster" — we compare medians rather than means to keep the
/// signal robust under scheduler jitter, and use distinct commands per pair
/// so the miss path actually executes the worker each time.
#[tokio::test]
async fn cache_hit_faster_than_miss() {
    let (endpoint, _dir) = boot_cluster().await;
    let mut client = BrokkrClient::connect(endpoint).await.unwrap();

    const N: usize = 10;
    let mut misses = Vec::with_capacity(N);
    let mut hits = Vec::with_capacity(N);

    for i in 0..N {
        let argv = vec!["/bin/echo".to_string(), format!("phase1-timing-{i}")];

        let t0 = Instant::now();
        let first = run_command(&mut client, &argv, false).await.unwrap();
        misses.push(t0.elapsed());
        assert!(!first.cache_hit, "expected miss on first run i={i}");

        let t1 = Instant::now();
        let second = run_command(&mut client, &argv, false).await.unwrap();
        hits.push(t1.elapsed());
        assert!(second.cache_hit, "expected hit on second run i={i}");
    }

    misses.sort();
    hits.sort();
    let median_miss = misses[N / 2];
    let median_hit = hits[N / 2];

    assert!(
        median_hit < median_miss,
        "cache hit must be measurably faster than miss; \
         median_hit={median_hit:?} median_miss={median_miss:?}"
    );
}
