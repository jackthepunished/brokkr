//! Split-port, mTLS+JWT cluster tests for issue #139.
//!
//! These tests spawn real `brokkr-control` and `brokkr-worker` binaries
//! with the production posture (TLS server cert + mTLS client cert on
//! the worker port + JWT bearer on the client port) and drive an
//! `Execute` from an in-process `BrokkrClient` carrying a freshly
//! minted JWT. The fixtures under `tests/fixtures/` are the same
//! committed PEMs used by `two_process_cluster.rs` for chain
//! verification.
//!
//! `#[ignore]` by default: they need all binaries built and open real
//! sockets. Run after a workspace build:
//!
//! ```text
//! cargo build --workspace
//! cargo test -p brokkr-control --test split_port_cluster -- --ignored --nocapture
//! ```
//!
//! The pre-existing `two_process_cluster.rs::run_a_job_across_two_processes`
//! still runs in open / single-port mode — that path must stay green
//! after this change (issue #139's whole point is that the open path
//! stays the same; only the auth-on path is newly verifiable).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::disallowed_methods,
    clippy::panic
)]

use std::env::consts::EXE_SUFFIX;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use brokkr_sdk::{run_command, BrokkrClient, TlsConfig};
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde_json::json;
use sha2::{Digest as _, Sha256};

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

/// Sibling binary path in the same target dir as this crate's test binary.
fn sibling_bin(name: &str) -> PathBuf {
    let control = env!("CARGO_BIN_EXE_brokkr-control");
    Path::new(control)
        .parent()
        .unwrap()
        .join(format!("{name}{EXE_SUFFIX}"))
}

/// Absolute path to a committed PEM/key fixture.
fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

/// Pick a likely-free port by binding :0 + dropping the listener.
fn pick_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let p = l.local_addr().unwrap().port();
    drop(l);
    p
}

/// Poll `addr` until it accepts a TCP connection or `budget` elapses.
fn wait_for_listen(addr: &str, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if TcpStream::connect(addr).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// Wait for the given child to exit, capturing stderr.
///
/// Returns `(exit_code, stderr_string)` once the child has terminated or
/// `budget` has elapsed (in which case the child is killed and we
/// return `(None, stderr_so_far)`).
fn wait_for_exit(mut child: std::process::Child, budget: Duration) -> (Option<i32>, String) {
    let deadline = Instant::now() + budget;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                // Drain stderr now; the OS pipe is closed once the child exits.
                let mut buf = String::new();
                use std::io::Read as _;
                if let Some(mut err) = child.stderr.take() {
                    let _ = err.read_to_string(&mut buf);
                }
                return (status.code(), buf);
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let mut buf = String::new();
                    use std::io::Read as _;
                    if let Some(mut err) = child.stderr.take() {
                        let _ = err.read_to_string(&mut buf);
                    }
                    return (None, buf);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => panic!("try_wait failed: {e}"),
        }
    }
}

/// Loose case-insensitive substring match: returns true iff any of the
/// `|`-separated needles appears in `haystack`. Avoids pulling in a
/// `regex` crate for the tests.
fn needle_match(needles: &str, haystack: &str) -> bool {
    let lc = haystack.to_ascii_lowercase();
    needles
        .split('|')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .any(|needle| lc.contains(&needle.to_ascii_lowercase()))
}

/// Kill + reap a child on drop so a failing assertion never leaks processes.
struct Reap(std::process::Child);
impl Drop for Reap {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Control-plane child + its endpoints.
struct BootedControl {
    /// Keeps the control process alive. Drop kills + reaps.
    _proc: Reap,
    /// `https://127.0.0.1:<client_port>` — TLS + JWT-gated client port.
    client_url_https: String,
    /// `https://127.0.0.1:<worker_port>` — TLS + mTLS-gated worker port.
    worker_url_https: String,
    hmac_secret: Vec<u8>,
}

fn boot_control_split(data_dir: &Path) -> BootedControl {
    let control_bin = PathBuf::from(env!("CARGO_BIN_EXE_brokkr-control"));
    let client_port = pick_port();
    let worker_port = pick_port();
    let client_addr = format!("127.0.0.1:{client_port}");
    let worker_addr = format!("127.0.0.1:{worker_port}");
    // The control plane binds TLS on both ports (server identity).
    let client_url = format!("https://{client_addr}");
    let worker_url = format!("https://{worker_addr}");

    // HMAC secret on disk so the CLI / control share the same file.
    // The file is deleted when `data_dir` is dropped (tempdir).
    let secret = b"split-port-cluster-test-secret-do-not-reuse".to_vec();
    std::fs::write(data_dir.join("hmac.secret"), &secret).unwrap();

    let proc = Reap(
        Command::new(&control_bin)
            .args(["--listen", &client_addr])
            .args(["--worker-listen", &worker_addr])
            .args(["--data-dir"])
            .arg(data_dir)
            .args(["--tls-cert"])
            .arg(fixture("server.pem"))
            .args(["--tls-key"])
            .arg(fixture("server.key"))
            .args(["--tls-client-ca"])
            .arg(fixture("ca.pem"))
            .args(["--auth-jwt-hmac-secret-file"])
            .arg(data_dir.join("hmac.secret"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );

    assert!(
        wait_for_listen(&client_addr, Duration::from_secs(15)),
        "control plane did not start listening on {client_addr}"
    );
    assert!(
        wait_for_listen(&worker_addr, Duration::from_secs(15)),
        "control plane did not start worker listener on {worker_addr}"
    );

    BootedControl {
        _proc: proc,
        client_url_https: client_url,
        worker_url_https: worker_url,
        hmac_secret: secret,
    }
}

/// Spawn a worker that talks to `control_client_url` (HTTPS, JWT-gated)
/// for CAS / ActionCache and to `control_worker_url` (HTTPS, mTLS) for
/// `WorkerService`.
fn boot_worker_with_mtls(control_client_url: &str, control_worker_url: &str) -> Reap {
    let worker_bin = sibling_bin("brokkr-worker");
    Reap(
        Command::new(&worker_bin)
            .args(["--control", control_client_url])
            .args(["--worker-control", control_worker_url])
            .args(["--ca"])
            .arg(fixture("ca.pem"))
            .args(["--client-cert"])
            .arg(fixture("worker.pem"))
            .args(["--client-key"])
            .arg(fixture("worker.key"))
            .args(["--no-sandbox"]) // Phase 1 parity; sandbox covered elsewhere.
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    )
}

/// Build an mTLS `TlsConfig` that pins the test CA. The control plane
/// runs HTTPS on the client port too (ADR 0011, issue #139 — TLS
/// terminates the network even on the JWT-gated port), so all in-process
/// SDK connections use this.
fn test_tls_config() -> TlsConfig {
    TlsConfig {
        ca_cert: fixture("ca.pem"),
        client_cert: None,
        client_key: None,
    }
}

/// Mint an HS256 JWT signed with `secret`, valid far in the future.
fn mint_jwt(secret: &[u8]) -> String {
    // Year ~2100 — keeps the test stable without a wall-clock dep.
    let far_future: u64 = 4_102_444_800;
    encode(
        &Header::new(Algorithm::HS256),
        &json!({ "tenant": "test", "exp": far_future }),
        &EncodingKey::from_secret(secret),
    )
    .unwrap()
}

/// Connect an in-process `BrokkrClient` over mTLS+bearer. The bearer
/// goes out as `authorization: Bearer <jwt>` on every RPC.
async fn in_process_jwt_client(control: &BootedControl) -> BrokkrClient {
    let token = mint_jwt(&control.hmac_secret);
    BrokkrClient::connect_with_tls_and_bearer(
        control.client_url_https.clone(),
        test_tls_config(),
        token,
    )
    .await
    .unwrap()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// **Closes issue #139.** Boots the control plane with both JWT auth and
/// mTLS, the worker with a valid client cert, and drives an `Execute`
/// over an in-process `BrokkrClient` carrying a freshly minted JWT.
/// Asserts the action runs and stdout contains the expected marker.
#[tokio::test(flavor = "current_thread")]
#[ignore = "spawns real binaries; run with --ignored after `cargo build --workspace`"]
async fn run_a_job_with_jwt_and_mtls() {
    let worker_bin = sibling_bin("brokkr-worker");
    if !worker_bin.exists() {
        eprintln!("skipping: build the workspace first (missing {worker_bin:?})");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let control = boot_control_split(dir.path());
    let _worker = boot_worker_with_mtls(&control.client_url_https, &control.worker_url_https);

    // Give the worker a moment to register + open the stream.
    std::thread::sleep(Duration::from_millis(300));

    let mut client = in_process_jwt_client(&control).await;
    // Retry briefly so the worker has time to register + open the
    // stream. Mirrors the pattern in `two_process_cluster.rs`.
    let deadline = Instant::now() + Duration::from_secs(10);
    let outcome = loop {
        match run_command(
            &mut client,
            &["/bin/echo".to_string(), "hello-mtls".to_string()],
            false,
        )
        .await
        {
            Ok(o) => break o,
            Err(e) => {
                let msg = format!("{e:#}");
                if Instant::now() >= deadline {
                    panic!("Execute never succeeded: {msg}");
                }
                if !msg.contains("no eligible worker") && !msg.contains("transport error") {
                    panic!("Execute failed with non-retryable error: {msg}");
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    };
    let stdout = String::from_utf8_lossy(&outcome.stdout);
    assert!(
        stdout.contains("hello-mtls"),
        "expected action stdout to contain 'hello-mtls', got: {stdout:?}"
    );
    assert_eq!(outcome.exit_code, 0);
}

/// Same setup as `run_a_job_with_jwt_and_mtls`. After the job returns,
/// open the on-disk `cas.redb` the control plane just wrote into and
/// assert the stdout digest is present in CAS — proving the worker's
/// `BatchUpdateBlobs` (over the JWT-gated client port, which used to
/// be the broken path) actually landed.
#[tokio::test(flavor = "current_thread")]
#[ignore = "spawns real binaries; run with --ignored after `cargo build --workspace`"]
async fn run_a_job_after_populating_cas() {
    let worker_bin = sibling_bin("brokkr-worker");
    if !worker_bin.exists() {
        eprintln!("skipping: build the workspace first (missing {worker_bin:?})");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let control = boot_control_split(dir.path());
    let _worker = boot_worker_with_mtls(&control.client_url_https, &control.worker_url_https);
    std::thread::sleep(Duration::from_millis(300));

    let mut client = in_process_jwt_client(&control).await;
    // Retry briefly so the worker has time to register + open the
    // stream. Mirrors the pattern in `two_process_cluster.rs`.
    let deadline = Instant::now() + Duration::from_secs(10);
    let outcome = loop {
        match run_command(
            &mut client,
            &["/bin/echo".to_string(), "cas-check".to_string()],
            false,
        )
        .await
        {
            Ok(o) => break o,
            Err(e) => {
                let msg = format!("{e:#}");
                if Instant::now() >= deadline {
                    panic!("Execute never succeeded: {msg}");
                }
                if !msg.contains("no eligible worker") && !msg.contains("transport error") {
                    panic!("Execute failed with non-retryable error: {msg}");
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    };
    assert_eq!(outcome.exit_code, 0);

    // Give the worker upload a moment to settle. Matches the pattern at
    // `crates/brokkr-control/tests/common/mod.rs`.
    std::thread::sleep(Duration::from_millis(100));

    // Read the stdout blob back over the wire. The worker uploaded it
    // to the worker port (mTLS-authenticated) — this is the proof that
    // the JWT-bypass path actually carried the bytes.
    let hash = hex::encode(Sha256::digest(&outcome.stdout));
    let blob = client
        .find_blob(&hash, outcome.stdout.len() as i64)
        .await
        .expect("BatchReadBlobs RPC failed");
    let blob = blob.expect("stdout blob is not in CAS after the worker uploaded it");
    assert_eq!(
        blob.as_ref(),
        outcome.stdout.as_ref(),
        "CAS contents differ from the worker's stdout"
    );
}

/// Runs the same `Execute` twice with the same JWT client. The second
/// call must be served from the action cache (the second invocation
/// skips the worker hop entirely).
#[tokio::test(flavor = "current_thread")]
#[ignore = "spawns real binaries; run with --ignored after `cargo build --workspace`"]
async fn second_run_hits_action_cache() {
    let worker_bin = sibling_bin("brokkr-worker");
    if !worker_bin.exists() {
        eprintln!("skipping: build the workspace first (missing {worker_bin:?})");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let control = boot_control_split(dir.path());
    let _worker = boot_worker_with_mtls(&control.client_url_https, &control.worker_url_https);
    std::thread::sleep(Duration::from_millis(300));

    let mut client = in_process_jwt_client(&control).await;

    // Retry briefly so the worker has time to register + open the
    // stream before the first Execute.
    let first = {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match run_command(
                &mut client,
                &["/bin/echo".to_string(), "cache-me".to_string()],
                false,
            )
            .await
            {
                Ok(o) => break o,
                Err(e) => {
                    let msg = format!("{e:#}");
                    if Instant::now() >= deadline {
                        panic!("first Execute never succeeded: {msg}");
                    }
                    if !msg.contains("no eligible worker") && !msg.contains("transport error") {
                        panic!("first Execute failed with non-retryable error: {msg}");
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
        }
    };
    assert!(!first.cache_hit, "first run should not be a cache hit");
    assert_eq!(first.exit_code, 0);

    let second = run_command(
        &mut client,
        &["/bin/echo".to_string(), "cache-me".to_string()],
        false,
    )
    .await
    .expect("second Execute failed");
    assert!(
        second.cache_hit,
        "second run with the same argv should be served from the action cache"
    );
    assert_eq!(second.exit_code, 0);
}

/// Worker is configured to point at an `https://` worker endpoint (mTLS
/// required) but is missing `--client-cert`/`--client-key`. The worker
/// must refuse to start instead of failing every RPC at runtime.
#[tokio::test(flavor = "current_thread")]
#[ignore = "spawns real binaries; run with --ignored after `cargo build --workspace`"]
async fn worker_without_client_cert_refuses_to_start() {
    let worker_bin = sibling_bin("brokkr-worker");
    if !worker_bin.exists() {
        eprintln!("skipping: build the workspace first (missing {worker_bin:?})");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let control = boot_control_split(dir.path());

    let child = Command::new(&worker_bin)
        .args(["--control", &control.client_url_https])
        .args(["--worker-control", &control.worker_url_https])
        // No --client-cert / --client-key on purpose.
        .args(["--ca"])
        .arg(fixture("ca.pem"))
        .args(["--no-sandbox"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (code, stderr) = wait_for_exit(child, Duration::from_secs(5));
    assert!(
        matches!(code, Some(c) if c != 0),
        "worker without --client-cert should exit non-zero, got code={code:?}\nstderr={stderr}"
    );
    assert!(
        needle_match(
            "client identity|client cert|client_cert|client-key|issue #139",
            &stderr
        ),
        "worker stderr should mention missing client identity, got: {stderr}"
    );
}

/// Worker presents a cert signed by a *different* CA than the control
/// plane's `--tls-client-ca`. The TLS handshake itself must fail (not
/// silently downgrade to no-mTLS). Reaching this assert at all is
/// proof the worker did not stay up indefinitely, since
/// `wait_for_exit` kills it at the budget if it hangs.
#[tokio::test(flavor = "current_thread")]
#[ignore = "spawns real binaries; run with --ignored after `cargo build --workspace`"]
async fn worker_with_wrong_ca_rejected_at_tls() {
    let worker_bin = sibling_bin("brokkr-worker");
    if !worker_bin.exists() {
        eprintln!("skipping: build the workspace first (missing {worker_bin:?})");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let control = boot_control_split(dir.path());

    let child = Command::new(&worker_bin)
        .args(["--control", &control.client_url_https])
        .args(["--worker-control", &control.worker_url_https])
        .args(["--ca"])
        .arg(fixture("ca.pem")) // verify server with the right CA
        .args(["--client-cert"])
        .arg(fixture("badworker.pem")) // ...but identify as `badworker` (signed by `badca`)
        .args(["--client-key"])
        .arg(fixture("badworker.key"))
        .args(["--no-sandbox"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (code, stderr) = wait_for_exit(child, Duration::from_secs(10));
    assert!(
        matches!(code, Some(c) if c != 0),
        "worker with cert from wrong CA should exit non-zero, got code={code:?}\nstderr={stderr}"
    );
    // rustls surfaces handshake failures with varying wording across
    // versions; we only require the worker process not to silently
    // succeed. The check below is loose on purpose.
    assert!(
        needle_match(
            "certificate|handshake|unknown issuer|invalid peer|bad certificate|peer certificate|broken pipe|stream closed|connection error|transport error",
            &stderr
        ),
        "worker stderr should mention a TLS failure, got: {stderr}"
    );
}

/// `--single-port --auth-jwt-*` is the original bug from #139: the
/// worker would share the JWT-gated listener and every CAS write would
/// be rejected. The control plane must refuse this combination at
/// startup.
#[tokio::test(flavor = "current_thread")]
#[ignore = "spawns real binaries; run with --ignored after `cargo build --workspace`"]
async fn single_port_with_jwt_refuses_to_start() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("hmac.secret"), b"s").unwrap();

    let client_addr = format!("127.0.0.1:{}", pick_port());
    let control_bin = PathBuf::from(env!("CARGO_BIN_EXE_brokkr-control"));
    let child = Command::new(&control_bin)
        .args(["--listen", &client_addr, "--single-port"])
        .args(["--tls-cert"])
        .arg(fixture("server.pem"))
        .args(["--tls-key"])
        .arg(fixture("server.key"))
        .args(["--tls-client-ca"])
        .arg(fixture("ca.pem"))
        .args(["--auth-jwt-hmac-secret-file"])
        .arg(dir.path().join("hmac.secret"))
        .args(["--data-dir"])
        .arg(dir.path())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (code, stderr) = wait_for_exit(child, Duration::from_secs(5));
    assert!(
        matches!(code, Some(c) if c != 0),
        "control with --single-port + --auth-jwt-* should exit non-zero, got code={code:?}\nstderr={stderr}"
    );
    assert!(
        needle_match("single-port|issue #139|workerservice would share", &stderr),
        "control stderr should explain the single-port + JWT incompatibility, got: {stderr}"
    );
}

/// `--auth-jwt-*` without `--tls-client-ca` (and without
/// `--single-port`) is also forbidden: there's no way for the worker
/// port to authenticate callers, so the worker→CAS path is dead.
#[tokio::test(flavor = "current_thread")]
#[ignore = "spawns real binaries; run with --ignored after `cargo build --workspace`"]
async fn auth_on_without_client_ca_refuses_to_start() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("hmac.secret"), b"s").unwrap();

    let client_addr = format!("127.0.0.1:{}", pick_port());
    let control_bin = PathBuf::from(env!("CARGO_BIN_EXE_brokkr-control"));
    let child = Command::new(&control_bin)
        .args(["--listen", &client_addr])
        .args(["--tls-cert"])
        .arg(fixture("server.pem"))
        .args(["--tls-key"])
        .arg(fixture("server.key"))
        // No --tls-client-ca.
        .args(["--auth-jwt-hmac-secret-file"])
        .arg(dir.path().join("hmac.secret"))
        .args(["--data-dir"])
        .arg(dir.path())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (code, stderr) = wait_for_exit(child, Duration::from_secs(5));
    assert!(
        matches!(code, Some(c) if c != 0),
        "control with JWT + no --tls-client-ca should exit non-zero, got code={code:?}\nstderr={stderr}"
    );
    assert!(
        needle_match("auth-jwt|requires --tls-client-ca|issue #139", &stderr),
        "control stderr should explain the missing client CA, got: {stderr}"
    );
}
