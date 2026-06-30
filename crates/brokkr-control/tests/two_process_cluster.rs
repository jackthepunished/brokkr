//! Two-process cluster smoke test: spawn the real `brokkr-control` and
//! `brokkr-worker` binaries as separate OS processes and run a job through the
//! `brokk` CLI end-to-end. Unlike the in-process fixtures (which exercise the
//! gRPC path within one process), this proves the binaries actually start, the
//! worker registers + connects over a real socket across process boundaries,
//! and `brokk run` returns the action's output and exit code.
//!
//! `#[ignore]` by default: it needs all three binaries built and spawns
//! processes. Run after a workspace build:
//!
//! ```text
//! cargo build --workspace
//! cargo test -p brokkr-control --test two_process_cluster -- --ignored --nocapture
//! ```

#![allow(clippy::unwrap_used, clippy::disallowed_methods, clippy::panic)]

use std::env::consts::EXE_SUFFIX;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// Sibling binary path in the same target dir as this crate's test binary.
fn sibling_bin(name: &str) -> PathBuf {
    let control = env!("CARGO_BIN_EXE_brokkr-control");
    Path::new(control)
        .parent()
        .unwrap()
        .join(format!("{name}{EXE_SUFFIX}"))
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

/// Kill + reap a child on drop so a failing assertion never leaks processes.
struct Reap(std::process::Child);
impl Drop for Reap {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
#[ignore = "spawns real binaries; run with --ignored after `cargo build --workspace`"]
async fn run_a_job_across_two_processes() {
    let worker_bin = sibling_bin("brokkr-worker");
    let brokk_bin = sibling_bin("brokk");
    let control_bin = PathBuf::from(env!("CARGO_BIN_EXE_brokkr-control"));
    // Skip cleanly if the sibling binaries weren't built (e.g. plain
    // `cargo test -p brokkr-control` without a workspace build).
    if !worker_bin.exists() || !brokk_bin.exists() {
        eprintln!("skipping: build the workspace first (missing {worker_bin:?} / {brokk_bin:?})");
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    // A likely-free port: bind :0, read it, drop the listener, reuse it.
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let addr = format!("127.0.0.1:{port}");
    let endpoint = format!("http://{addr}");

    // Control plane (open mode: no TLS / no auth configured).
    let _control = Reap(
        Command::new(&control_bin)
            .args(["--listen", &addr, "--data-dir"])
            .arg(dir.path())
            .spawn()
            .unwrap(),
    );
    assert!(
        wait_for_listen(&addr, Duration::from_secs(15)),
        "control plane did not start listening on {addr}"
    );

    // Worker (plain runner — no sandbox, so it runs /bin/echo directly).
    let _worker = Reap(
        Command::new(&worker_bin)
            .args(["--control", &endpoint, "--no-sandbox"])
            .spawn()
            .unwrap(),
    );

    // `brokk run` — retry briefly so the worker has time to register + connect.
    let deadline = Instant::now() + Duration::from_secs(15);
    let output = loop {
        let out = Command::new(&brokk_bin)
            .args([
                "run",
                "--control",
                &endpoint,
                "--",
                "/bin/echo",
                "hello-cluster",
            ])
            .output()
            .unwrap();
        if out.status.success() {
            break out;
        }
        if Instant::now() >= deadline {
            panic!(
                "brokk run never succeeded: status={:?}\nstdout={}\nstderr={}",
                out.status,
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(200));
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("hello-cluster"),
        "expected action stdout in `brokk run` output, got: {stdout:?}"
    );
}
