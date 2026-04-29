//! End-to-end Phase 1 test: control plane + worker + SDK in-process.
//!
//! Spins up the gRPC server and a worker over an ephemeral TCP port, runs
//! `echo hello world` via the SDK, then runs the same command again to assert
//! a cache hit. Phase 1 exit criterion (plan §13).

#![allow(clippy::unwrap_used, clippy::panic, clippy::disallowed_methods)]

use brokkr_sdk::{run_command, BrokkrClient};

mod common;
use common::boot_cluster;

#[tokio::test]
async fn echo_hello_world_runs_and_caches() {
    let (endpoint, _dir) = boot_cluster().await;
    let mut client = BrokkrClient::connect(endpoint).await.unwrap();

    let argv = vec!["/bin/echo".to_string(), "hello world".to_string()];

    let first = run_command(&mut client, &argv, false).await.unwrap();
    assert_eq!(first.exit_code, 0, "first run exit code");
    assert!(
        !first.cache_hit,
        "first run must not be cached, got cache_hit=true"
    );
    assert_eq!(first.stdout.as_ref(), b"hello world\n");

    let second = run_command(&mut client, &argv, false).await.unwrap();
    assert_eq!(second.exit_code, 0, "second run exit code");
    assert!(
        second.cache_hit,
        "second run must hit the action cache, got cache_hit=false"
    );
    assert_eq!(second.stdout.as_ref(), b"hello world\n");
}

#[tokio::test]
async fn nonzero_exit_is_not_cached() {
    let (endpoint, _dir) = boot_cluster().await;
    let mut client = BrokkrClient::connect(endpoint).await.unwrap();
    let argv = vec!["/bin/false".to_string()];
    let first = run_command(&mut client, &argv, false).await.unwrap();
    assert_ne!(first.exit_code, 0);
    assert!(!first.cache_hit);

    // Second run must also miss because exit != 0 isn't cached.
    let second = run_command(&mut client, &argv, false).await.unwrap();
    assert!(!second.cache_hit, "failures must not be cached");
}
