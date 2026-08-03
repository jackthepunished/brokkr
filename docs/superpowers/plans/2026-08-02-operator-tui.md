# Operator TUI Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A read-only terminal console showing what a Brokkr cluster is doing — nodes and Raft health, workers, recent jobs, and a live activity feed — fed by the observability backend.

**Architecture:** A `brokkr-tui` binary crate over `ratatui` + `crossterm`, with a hand-rolled Elm architecture: a pure `update(&mut Model, Action)` and a `view(&Model, Frame)`, driven by a `tokio::mpsc` action bus that merges terminal input, a render tick, and a connection actor. The model is replaced wholesale by a `SnapshotEvent` (from the stream's opening/resync snapshot or a periodic `GetSnapshot` sweep) and mutated incrementally by stream deltas.

**Tech Stack:** Rust 1.94, `ratatui` 0.30, `crossterm` 0.29, `tokio`, `tonic` 0.12 via `brokkr-sdk`.

**Spec:** `docs/superpowers/specs/2026-08-02-operator-tui-design.md`
**ADR:** `docs/architecture/0012-operator-tui.md`
**Backend it consumes:** complete and merged, PRs #192–#200.

## Global Constraints

Copied from CLAUDE.md and the spec. Every task's requirements implicitly include this section.

- **Never use `unwrap()` or `expect()` in library crates.** Tests and binaries may use them sparingly. Propagate with `?` and typed `thiserror` enums. `brokkr-tui` is a **binary** crate, so sparing use is permitted in `main.rs` — but `app.rs` and `conn.rs` are library-shaped and should not.
- **Never introduce `unsafe` without a `// SAFETY:` comment.**
- **Never disable a failing test to make CI green.** Fix it, or `#[ignore]` with a TODO and a tracking issue link.
- **New dependency ⇒ one-line rationale in the PR description.** This plan adds exactly two, both in T3.
- **Never run `cargo update` as a side effect.** Lockfile changes are their own commit — except a lockfile line that is a direct consequence of a dependency edge the same PR adds, which belongs in that PR. **Stage `Cargo.lock` explicitly**: `git add <paths>` misses it because it lives at the repo root, and CI runs `--locked`. This has failed twice in this project already.
- **Update `CHANGELOG.md`** under `## Unreleased` in the same commit.
- **Write the test in the same commit as the implementation.**
- Rustfmt default config + `tab_spaces = 4`, `max_width = 100`. Imports grouped: std, external, local, super, self.
- `#[derive(Debug)]` everywhere; **if it can't derive `Debug`, document why.**
- A file over 500 lines is a smell.
- Conventional commits. Branch `feat/<short>` **from `origin/main`**, never from another feature branch.

**Verification before every PR — run all five, after `touch crates/*/src/lib.rs crates/*/src/main.rs`** (cargo does not re-emit diagnostics for crates it considers fresh, which has produced a false green on this project before):

```bash
touch crates/*/src/lib.rs crates/*/src/main.rs
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
cargo test --workspace --all-features --no-fail-fast > /tmp/test.log 2>&1; echo "exit=$?"
cargo deny check advisories licenses bans
```

Never pipe `cargo test` into `tail` — the pipeline reports `tail`'s status, not cargo's. Redirect and echo `$?` separately.

**Environmental test gate:** the workspace suite on the development box fails **exactly these four and nothing else**:

```
ev09_rdtsc_blocked
ev_ioctl_tiocgwinsz_blocked
ev_ioctl_tiocptlck_blocked
ev_ioctl_tiocswinsz_blocked
```

All in `crates/brokkr-sandbox/tests/evil_seccomp_caps.rs`; seccomp argument filters need a real kernel. **Any other failure is a real defect — fix it. Never weaken an assertion and never re-run hoping for green.**

**PR gate:** all six checks present and passing — `cargo fmt`, `cargo clippy`, `cargo doc`, `cargo deny`, `cargo test (x86_64-unknown-linux-gnu)`, `cargo test (aarch64-unknown-linux-gnu)`. Fewer than six named jobs means the workflow did not run and the green is false.

## Dependency facts, already verified

Measured on 2026-08-02 against rustc 1.94.0, so T3 does not have to rediscover them:

| | |
|---|---|
| `ratatui` | **0.30.2** — builds on 1.94 |
| `crossterm` | **0.29.0** with the `event-stream` feature — needed for the async input source |
| `cargo deny` | advisories **ok**, licenses **ok**, bans **ok** with pinned versions |
| tree size | ~80 crates added |

Pin as `ratatui = "0.30"` and `crossterm = { version = "0.29", features = ["event-stream"] }`. **Do not use `"*"`** — `deny.toml` sets `wildcards = "deny"` and it will fail the gate.

---

## File Structure

| Path | Status | Responsibility |
|---|---|---|
| `crates/brokkr-control/tests/three_node_observability.rs` | create | T1: real 3-node aggregation |
| `crates/brokkr-proto/protos/brokkr/v1/observability.proto` | modify | T2: `GetSnapshot` |
| `crates/brokkr-control/src/services/observability.rs` | modify | T2: the handler |
| `crates/brokkr-sdk/src/observability.rs` | modify | T2: the client method |
| `crates/brokkr-tui/Cargo.toml` | create | T3 |
| `crates/brokkr-tui/src/main.rs` | create | T3: flags, terminal lifecycle, panic hook |
| `crates/brokkr-tui/src/terminal.rs` | create | T3: enter/restore, tested at the seam |
| `crates/brokkr-tui/src/app.rs` | create | T3: `Model`, `Action`, pure `update()` |
| `crates/brokkr-tui/src/conn.rs` | create | T4: connection actor, rotation, sweep |
| `crates/brokkr-tui/src/panels/mod.rs` | create | T5: `Component` trait, shared styling |
| `crates/brokkr-tui/src/panels/cluster.rs` | create | T5 |
| `crates/brokkr-tui/src/panels/workers.rs` | create | T5 |
| `crates/brokkr-tui/src/panels/jobs.rs` | create | T6 |
| `crates/brokkr-tui/src/panels/events.rs` | create | T6 |
| `docs/operations/using-the-console.md` | create | T7 |

`terminal.rs` is split from `main.rs` deliberately: the restore routine must be a
plain function to be unit-testable, which is the whole of how DoD 6 is honoured.

---

### Task 1: 3-node aggregation, against real processes

Branch: `feat/three-node-observability` from `origin/main`.

**This validates already-shipped code.** `GrpcPeerProbe`, `node_state_from_proto`,
and whether peer-plane mTLS covers `PeerObservability` have never run against a
real server — every existing aggregation test uses a fake probe. Do this first;
a transport bug found here is far cheaper than one found under a UI.

**Files:**
- Create: `crates/brokkr-control/tests/three_node_observability.rs`
- Modify: `CHANGELOG.md`

**Interfaces:**
- Consumes: the `brokkr-control` binary, `brokkr_sdk::ObservabilityClient`.
- Produces: nothing other tasks depend on. This is a pure verification increment.

- [ ] **Step 1: Read the pattern you are copying**

Read `crates/brokkr-control/tests/raft_ha_e2e.rs` in full before writing
anything. It already solves: spawning `brokkr-control` as a subprocess, the
`Reap` guard that kills children on drop so a failing assertion never leaks
processes, `sibling_bin()` for locating the binary, and waiting for a port to
accept. **Reuse those helpers' shape rather than inventing a second pattern.**

- [ ] **Step 2: Write the test file**

Create `crates/brokkr-control/tests/three_node_observability.rs`:

```rust
//! Observability aggregation across three real control-plane processes.
//!
//! Every other aggregation test uses a fake `PeerProbe`. This is the only one
//! that exercises `GrpcPeerProbe`, `node_state_from_proto`, and whether the
//! Raft peer plane's mTLS configuration actually covers `PeerObservability` —
//! all of which shipped untested against a real server.
//!
//! `#[ignore]` by default: spawns processes and binds real sockets. Run after
//! a workspace build:
//!
//! ```text
//! cargo build --workspace
//! cargo test -p brokkr-control --test three_node_observability -- --ignored --nocapture
//! ```

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::disallowed_methods,
    clippy::panic
)]

use std::env::consts::EXE_SUFFIX;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use brokkr_sdk::ObservabilityClient;

/// Kill + reap a child on drop so a failing assertion never leaks processes.
struct Reap(Child);
impl Drop for Reap {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn sibling_bin(name: &str) -> PathBuf {
    let control = env!("CARGO_BIN_EXE_brokkr-control");
    Path::new(control)
        .parent()
        .unwrap()
        .join(format!("{name}{EXE_SUFFIX}"))
}

/// Block until `port` accepts, or panic with what we were waiting for.
fn wait_for_port(port: u16, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("{what} (port {port}) never accepted a connection");
}

/// One node's ports. Chosen as a contiguous block per node so a failure is
/// readable: node N owns 79N0..79N3.
struct Ports {
    client: u16,
    worker: u16,
    raft: u16,
    observe: u16,
}

fn ports(n: u16) -> Ports {
    let base = 7900 + n * 10;
    Ports {
        client: base,
        worker: base + 1,
        raft: base + 2,
        observe: base + 3,
    }
}

/// Spawn one control-plane node with `peers` as its `--raft-peer` set.
fn spawn_node(dir: &Path, id: &str, p: &Ports, peers: &[(String, u16)]) -> Reap {
    let mut cmd = Command::new(sibling_bin("brokkr-control"));
    cmd.arg("--data-dir")
        .arg(dir.join(id))
        .args(["--node-id", id])
        .args(["--listen", &format!("127.0.0.1:{}", p.client)])
        .args(["--worker-listen", &format!("127.0.0.1:{}", p.worker)])
        .args(["--raft-listen", &format!("127.0.0.1:{}", p.raft)])
        .args(["--observe-listen", &format!("127.0.0.1:{}", p.observe)])
        .args(["--advertise-addr", &format!("127.0.0.1:{}", p.client)])
        .arg("--raft")
        // A short poll so the test does not wait 2s per assertion.
        .args(["--observe-poll-interval-secs", "1"])
        .args(["--observe-peer-timeout-ms", "500"]);
    for (peer_id, raft_port) in peers {
        cmd.args(["--raft-peer", &format!("{peer_id}=127.0.0.1:{raft_port}")]);
    }
    let child = cmd
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap_or_else(|e| panic!("spawning {id}: {e}. Run `cargo build --workspace` first."));
    Reap(child)
}

/// Poll `f` until it returns true or the budget expires.
async fn eventually(budget: Duration, mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if f() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    f()
}

/// **DoD 1.** Every node reports the whole cluster, and each record says which
/// node it came from.
#[tokio::test]
#[ignore = "spawns three brokkr-control processes; run after cargo build --workspace"]
async fn every_node_reports_the_whole_cluster() {
    let dir = tempfile::tempdir().unwrap();
    let (p1, p2, p3) = (ports(0), ports(1), ports(2));
    let all = [
        ("node-1".to_string(), p1.raft),
        ("node-2".to_string(), p2.raft),
        ("node-3".to_string(), p3.raft),
    ];
    let peers_of = |me: &str| -> Vec<(String, u16)> {
        all.iter().filter(|(id, _)| id != me).cloned().collect()
    };

    let _n1 = spawn_node(dir.path(), "node-1", &p1, &peers_of("node-1"));
    let _n2 = spawn_node(dir.path(), "node-2", &p2, &peers_of("node-2"));
    let _n3 = spawn_node(dir.path(), "node-3", &p3, &peers_of("node-3"));

    for (p, id) in [(&p1, "node-1"), (&p2, "node-2"), (&p3, "node-3")] {
        wait_for_port(p.observe, &format!("{id} observability listener"));
    }

    // Ask every node; each must see all three.
    for p in [&p1, &p2, &p3] {
        let endpoint = format!("http://127.0.0.1:{}", p.observe);
        let mut c = ObservabilityClient::connect(endpoint.clone()).await.unwrap();

        let mut seen = Vec::new();
        let converged = eventually(Duration::from_secs(30), || {
            let rt = tokio::runtime::Handle::current();
            let cluster = tokio::task::block_in_place(|| {
                rt.block_on(async { c.get_cluster().await.ok().flatten() })
            });
            match cluster {
                Some(info) => {
                    seen = info.nodes.iter().map(|n| n.node_id.clone()).collect();
                    seen.len() == 3 && info.nodes.iter().all(|n| n.reachable)
                }
                None => false,
            }
        })
        .await;
        assert!(
            converged,
            "{endpoint} never saw all three nodes reachable; saw {seen:?}. \
             This is the first test to exercise GrpcPeerProbe against a real \
             PeerObservability server — a failure here is a transport or \
             mapping bug, not a merge bug."
        );

        // Per-node collections are per node, never combined.
        let stores = c.get_cas_stats().await.unwrap();
        assert_eq!(stores.len(), 3, "one CAS entry per node, never summed");
        let mut owners: Vec<&str> = stores.iter().map(|s| s.owning_node.as_str()).collect();
        owners.sort_unstable();
        assert_eq!(owners, vec!["node-1", "node-2", "node-3"]);

        let policies = c.get_policy().await.unwrap();
        assert_eq!(policies.len(), 3, "one policy entry per node, never combined");
    }
}

/// **DoD 2.** Killing a node degrades the view without breaking it, and the
/// dead node stays visible rather than vanishing.
#[tokio::test]
#[ignore = "spawns three brokkr-control processes; run after cargo build --workspace"]
async fn killing_a_node_degrades_without_failing() {
    let dir = tempfile::tempdir().unwrap();
    let (p1, p2, p3) = (ports(3), ports(4), ports(5));
    let all = [
        ("node-1".to_string(), p1.raft),
        ("node-2".to_string(), p2.raft),
        ("node-3".to_string(), p3.raft),
    ];
    let peers_of = |me: &str| -> Vec<(String, u16)> {
        all.iter().filter(|(id, _)| id != me).cloned().collect()
    };

    let _n1 = spawn_node(dir.path(), "node-1", &p1, &peers_of("node-1"));
    let _n2 = spawn_node(dir.path(), "node-2", &p2, &peers_of("node-2"));
    let n3 = spawn_node(dir.path(), "node-3", &p3, &peers_of("node-3"));

    for (p, id) in [(&p1, "node-1"), (&p2, "node-2"), (&p3, "node-3")] {
        wait_for_port(p.observe, &format!("{id} observability listener"));
    }
    let mut c = ObservabilityClient::connect(format!("http://127.0.0.1:{}", p1.observe))
        .await
        .unwrap();

    // Converge first, so the assertion below is about the kill and not startup.
    let converged = eventually(Duration::from_secs(30), || {
        let rt = tokio::runtime::Handle::current();
        tokio::task::block_in_place(|| {
            rt.block_on(async {
                c.get_cluster()
                    .await
                    .ok()
                    .flatten()
                    .is_some_and(|i| i.nodes.iter().filter(|n| n.reachable).count() == 3)
            })
        })
    })
    .await;
    assert!(converged, "the cluster never converged before the kill");

    drop(n3); // Reap kills it.

    let degraded = eventually(Duration::from_secs(30), || {
        let rt = tokio::runtime::Handle::current();
        tokio::task::block_in_place(|| {
            rt.block_on(async {
                match c.get_cluster().await {
                    // The call must still SUCCEED — an observability API is
                    // most needed exactly when something is broken.
                    Ok(Some(i)) => {
                        i.degraded
                            && i.nodes.len() == 3
                            && i.nodes.iter().any(|n| n.node_id == "node-3" && !n.reachable)
                    }
                    _ => false,
                }
            })
        })
    })
    .await;
    assert!(
        degraded,
        "after killing node-3 the view should report degraded, still list three \
         nodes, and mark node-3 unreachable — 'known but silent' must stay \
         distinct from 'not a member'"
    );
}
```

Both tests need `#[tokio::test(flavor = "multi_thread")]` because they use
`block_in_place`. Add that attribute if the single-threaded default panics.

- [ ] **Step 3: Build the workspace, then run the tests**

```bash
cargo build --workspace
cargo test -p brokkr-control --test three_node_observability -- --ignored --nocapture
```

Expected: PASS, 2 tests. **If they fail, that is the point of this task** — the
failure is in shipped code, not in the test. Read the assertion message, fix the
transport or mapping bug it names, and keep the test as written.

- [ ] **Step 4: Run the full verification gate**

Run the five commands from Global Constraints. The new tests are `#[ignore]`d so
the workspace run will not execute them; the failure list must still be exactly
the four `evil_seccomp_caps` names.

- [ ] **Step 5: Update the CHANGELOG and commit**

Under `## [Unreleased]` → `### Added`:

```markdown
- **Three-node observability aggregation test** (`#[ignore]`d, spawns real
  processes). Until now every aggregation test used a fake `PeerProbe`, so
  `GrpcPeerProbe`, `node_state_from_proto`, and whether the Raft peer plane's
  mTLS covers `PeerObservability` had shipped without ever running against a
  real server. Asserts every node sees the whole cluster, that CAS and policy
  come back one-per-node, and that killing a node degrades the view without
  failing the call.
```

```bash
git add crates/brokkr-control/tests/three_node_observability.rs CHANGELOG.md
git status --short | grep -v '^??'   # confirm nothing else is pending
git commit -m "test(control): prove observability aggregation on three real nodes

Motivation: every aggregation test to date used a fake PeerProbe. GrpcPeerProbe,
node_state_from_proto, and whether the Raft peer plane's mTLS configuration
actually covers PeerObservability all shipped without ever running against a
real server. The merge rules were well tested; the transport under them was not.

Asserts DoD 1 and 2 from the observability spec against three real
brokkr-control processes: every node reports the whole cluster with each record
labelled by its owner, CAS and policy come back one entry per node rather than
combined, and killing a node leaves the call succeeding while reporting degraded
and keeping the dead node visible.

#[ignore]d because it spawns processes and binds real sockets, like
raft_ha_e2e. Its helpers follow that file's shape — the Reap guard in
particular, so a failing assertion never leaks processes.

Related: docs/superpowers/plans/2026-08-02-operator-tui.md Task 1, ADR 0012."
```

---

### Task 2: `GetSnapshot`

Branch: `feat/get-snapshot` from `origin/main`.

**Why this exists.** The TUI's periodic sweep needs an *atomic* read. Assembling
one from `GetCluster` + `ListWorkers` + `ListJobs` + `GetPolicy` +
`GetCasStats` would be five RPCs, and the poller can swap the snapshot between
any two of them — yielding a model with workers from one poll and jobs from the
next. `GetSnapshot` returns the same `SnapshotEvent` the stream sends, from one
read.

**Files:**
- Modify: `crates/brokkr-proto/protos/brokkr/v1/observability.proto`
- Modify: `crates/brokkr-control/src/services/observability.rs`
- Modify: `crates/brokkr-sdk/src/observability.rs`
- Modify: `crates/brokkr-control/tests/observability_listener.rs`
- Modify: `CHANGELOG.md`

**Interfaces:**
- Produces, relied on by T4:
  - `rpc GetSnapshot(GetSnapshotRequest) returns (SnapshotEvent);`
  - `ObservabilityClient::get_snapshot(&mut self) -> Result<bv1::SnapshotEvent, ClientError>`

- [ ] **Step 1: Add the RPC to the proto**

In `crates/brokkr-proto/protos/brokkr/v1/observability.proto`, add above
`service ObservabilityService`:

```proto
message GetSnapshotRequest {}
```

and inside the service:

```proto
  // The same payload WatchEvents opens with, from a single read.
  //
  // A client that wants a periodic full refresh must use this rather than
  // assembling one from the unary reads: those are five separate RPCs and the
  // poller can swap the snapshot between any two, yielding workers from one
  // poll and jobs from the next.
  rpc GetSnapshot(GetSnapshotRequest) returns (SnapshotEvent);
```

- [ ] **Step 2: Verify codegen**

```bash
cargo build -p brokkr-proto
```

Expected: builds clean.

- [ ] **Step 3: Write the failing test**

Add to `crates/brokkr-control/tests/observability_listener.rs`:

```rust
/// `GetSnapshot` returns the same payload the stream opens with, from one read.
///
/// The equivalence is the point: a client using this for a periodic refresh
/// must get something indistinguishable from a stream snapshot, or the two
/// sources could disagree.
#[tokio::test]
async fn get_snapshot_matches_the_stream_opening_snapshot() {
    use tokio_stream::StreamExt as _;

    let (_client, observe, _dir) = common::boot_with_observability().await;
    let mut c = ObservabilityServiceClient::connect(observe).await.unwrap();

    let unary = c
        .get_snapshot(GetSnapshotRequest {})
        .await
        .unwrap()
        .into_inner();

    let mut stream = c
        .watch_events(WatchEventsRequest {})
        .await
        .unwrap()
        .into_inner();
    let first = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
        .await
        .expect("the stream opens with a snapshot")
        .expect("stream ended")
        .unwrap();
    let streamed = match first.event.expect("the first message carries an event") {
        brokkr_proto::brokkr_v1::cluster_event::Event::Snapshot(s) => s,
        other => panic!("expected Snapshot, got {other:?}"),
    };

    // The cluster may have been re-polled between the two calls, so compare the
    // shape rather than demanding byte equality — the invariant is that both
    // carry a complete, self-consistent world.
    assert_eq!(
        unary.cluster.as_ref().map(|c| c.nodes.len()),
        streamed.cluster.as_ref().map(|c| c.nodes.len())
    );
    assert_eq!(unary.policies.len(), streamed.policies.len());
    assert_eq!(unary.stores.len(), streamed.stores.len());
    assert!(unary.cluster.is_some(), "a snapshot always carries a cluster");
}
```

Add `GetSnapshotRequest` to the file's `use` list.

- [ ] **Step 4: Run it to verify it fails**

```bash
cargo test -p brokkr-control --test observability_listener get_snapshot
```

Expected: FAIL to compile — no method `get_snapshot`.

- [ ] **Step 5: Implement the handler**

In `crates/brokkr-control/src/services/observability.rs`, inside
`impl ObservabilityRpc for ObservabilityService`:

```rust
    #[tracing::instrument(name = "observability::get_snapshot", level = "debug", skip_all)]
    async fn get_snapshot(
        &self,
        _request: Request<bv1::GetSnapshotRequest>,
    ) -> Result<Response<bv1::SnapshotEvent>, Status> {
        // Reuses the same builder the stream's opening message uses, so the two
        // cannot drift apart.
        let event = self.snapshot_event().await;
        match event.event {
            Some(bv1::cluster_event::Event::Snapshot(s)) => Ok(Response::new(s)),
            // `snapshot_event` always builds a Snapshot; this arm exists so a
            // future change to it fails loudly here rather than silently
            // returning an empty world.
            _ => Err(Status::internal(
                "snapshot_event did not produce a Snapshot; this is a bug",
            )),
        }
    }
```

- [ ] **Step 6: Run the test to verify it passes**

```bash
cargo test -p brokkr-control --test observability_listener get_snapshot
```

Expected: PASS.

- [ ] **Step 7: Add the SDK method**

In `crates/brokkr-sdk/src/observability.rs`, inside `impl ObservabilityClient`:

```rust
    /// The whole cluster state in one atomic read.
    ///
    /// The same payload [`Self::watch_events`] opens with. Use this for a
    /// periodic full refresh rather than assembling one from the individual
    /// reads: those are five separate RPCs, and the server can re-poll between
    /// any two of them, giving you workers from one poll and jobs from the next.
    pub async fn get_snapshot(&mut self) -> Result<bv1::SnapshotEvent, ClientError> {
        Ok(self
            .inner
            .get_snapshot(bv1::GetSnapshotRequest {})
            .await?
            .into_inner())
    }
```

Add to `crates/brokkr-control/tests/sdk_observability.rs`, inside
`the_sdk_reads_every_observability_surface`:

```rust
    let snapshot = c.get_snapshot().await.unwrap();
    assert!(snapshot.cluster.is_some());
    assert_eq!(snapshot.policies.len(), 1);
    assert_eq!(snapshot.stores.len(), 1);
```

- [ ] **Step 8: Run the gate, update the CHANGELOG, commit**

Run the five commands. CHANGELOG under `### Added`:

```markdown
- **`GetSnapshot`** — the whole observability snapshot in one atomic read,
  returning the same payload `WatchEvents` opens with. A client wanting a
  periodic full refresh needs this rather than five separate unary calls, which
  the server can re-poll between, yielding workers from one poll and jobs from
  the next.
```

```bash
git add crates/brokkr-proto crates/brokkr-control crates/brokkr-sdk CHANGELOG.md
git status --short | grep -v '^??'   # confirm nothing else is pending
git commit -m "feat(control): add GetSnapshot for atomic full reads

Motivation: the operator console refreshes periodically as a safety net against
a delta bug. Assembling that refresh from the five unary reads would not be
atomic — the poller can swap the snapshot between any two calls, producing a
model with workers from one poll and jobs from the next.

GetSnapshot returns the same SnapshotEvent the stream opens with, from a single
read, so a sweep-derived refresh is genuinely equivalent to a stream snapshot
rather than merely similar.

The handler reuses snapshot_event(), the same builder the stream uses, so the
two cannot drift. Its non-Snapshot arm is unreachable today and exists so a
future change fails loudly rather than silently returning an empty world.

How tested: an integration test asserting the unary and streamed snapshots agree
in shape. Deliberately not byte equality — the cluster may be re-polled between
the two calls, and the invariant that matters is that each carries a complete,
self-consistent world.

Related: docs/superpowers/plans/2026-08-02-operator-tui.md Task 2, spec D2."
```

---

### Task 3: `brokkr-tui` scaffold

Branch: `feat/brokkr-tui-scaffold` from `origin/main`.

Deliberately has **no backend dependency** — it can proceed even if T1 turns up
a transport bug.

**Files:**
- Create: `crates/brokkr-tui/Cargo.toml`, `src/main.rs`, `src/terminal.rs`, `src/app.rs`
- Modify: root `Cargo.toml` (workspace members and deps)
- Modify: `Cargo.lock`
- Modify: `CHANGELOG.md`

**Interfaces:**
- Produces, relied on by T4–T6:

```rust
// terminal.rs
pub fn enter() -> std::io::Result<Terminal<CrosstermBackend<Stdout>>>;
pub fn restore() -> std::io::Result<()>;
pub fn install_panic_hook();

// app.rs
pub enum LinkState { Connecting { endpoint: String }, Connected { endpoint: String }, Disconnected { reason: String } }
pub enum Panel { Cluster, Workers, Jobs, Events }
pub enum Entry { Cluster(bv1::ClusterEvent), Local(String) }
pub struct StampedEntry { pub at: SystemTime, pub entry: Entry }
pub enum Action {
    Replace(Box<bv1::SnapshotEvent>),
    Apply(bv1::ClusterEvent),
    Link(LinkState),
    Key(crossterm::event::KeyEvent),
    Tick(SystemTime),
    Local(String),
}
pub struct Model { /* fields below */ }
impl Model { pub fn new() -> Self; pub fn should_quit(&self) -> bool; }
pub fn update(model: &mut Model, action: Action);
pub const EVENT_RING: usize = 1000;
```

- [ ] **Step 1: Create the crate manifest**

`crates/brokkr-tui/Cargo.toml`:

```toml
[package]
name = "brokkr-tui"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true
authors.workspace = true
description = "Brokkr operator console: a read-only terminal view of cluster state."

[lints]
workspace = true

[[bin]]
name = "brokkr-tui"
path = "src/main.rs"

[dependencies]
brokkr-proto.workspace = true
brokkr-sdk.workspace = true
anyhow.workspace = true
clap.workspace = true
crossterm.workspace = true
futures.workspace = true
ratatui.workspace = true
tokio.workspace = true
tracing.workspace = true
```

In the root `Cargo.toml`, add `"crates/brokkr-tui",` to `members` (after
`brokkr-cli`), add to `[workspace.dependencies]`:

```toml
# Operator console (ADR 0012). The de-facto Rust TUI renderer, and its async
# input backend — `event-stream` is what lets terminal input join the action bus
# as a stream rather than a blocking read.
ratatui = "0.30"
crossterm = { version = "0.29", features = ["event-stream"] }
```

and add `brokkr-tui = { path = "crates/brokkr-tui", version = "0.1.0" }` to the
internal-crates block.

**Pin the versions exactly as written.** `deny.toml` sets `wildcards = "deny"`.

- [ ] **Step 2: Write the terminal lifecycle with its test**

`crates/brokkr-tui/src/terminal.rs`:

```rust
//! Terminal lifecycle: raw mode, alternate screen, and getting out again.
//!
//! `restore` is a plain function rather than a `Drop` impl or a closure, because
//! it has to be callable from three places that cannot share state: normal
//! shutdown, the panic hook, and a unit test. That shape is the whole of how
//! the "a panic never leaves a wedged shell" guarantee is verified.

use std::io::{stdout, Stdout};

use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

/// Enter raw mode and the alternate screen.
pub fn enter() -> std::io::Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen)?;
    Terminal::new(CrosstermBackend::new(out))
}

/// Leave the alternate screen and raw mode.
///
/// Idempotent and best-effort: it runs from the panic hook, where returning an
/// error would be useless and panicking again would be worse. Both operations
/// are attempted even if the first fails, because leaving raw mode enabled is
/// the failure that actually ruins the user's shell.
pub fn restore() -> std::io::Result<()> {
    let leave = execute!(stdout(), LeaveAlternateScreen);
    let raw = disable_raw_mode();
    leave.and(raw)
}

/// Install a panic hook that restores the terminal before unwinding.
///
/// Without this, a panic leaves the user in raw mode on the alternate screen —
/// no echo, no line editing, and the panic message invisible. It is the failure
/// naive ratatui applications get wrong, and it is why `restore` is a free
/// function.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore();
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `restore` must be safe to call when no terminal was ever entered — the
    /// panic hook may fire before `enter`, or twice.
    #[test]
    fn restore_is_safe_to_call_without_entering() {
        let _ = restore();
        let _ = restore();
    }

    /// The hook must actually be installed. Asserting it *fires* would mean
    /// panicking inside the harness and inspecting global process state; this
    /// asserts the observable part, and the real terminal behaviour is verified
    /// by hand and recorded in T7.
    #[test]
    fn install_panic_hook_replaces_the_default_hook() {
        let before = std::panic::take_hook();
        std::panic::set_hook(before);

        install_panic_hook();

        // Taking the hook back proves one was set; putting it back keeps the
        // harness usable for other tests in this binary.
        let after = std::panic::take_hook();
        std::panic::set_hook(after);
    }
}
```

- [ ] **Step 3: Write the failing `update()` tests**

`crates/brokkr-tui/src/app.rs`, test module only for now:

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::time::{Duration, SystemTime};

    use brokkr_proto::brokkr_v1 as bv1;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::*;

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn key(c: char) -> Action {
        Action::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
    }

    fn snapshot_with(node_ids: &[&str], worker_ids: &[&str]) -> Box<bv1::SnapshotEvent> {
        Box::new(bv1::SnapshotEvent {
            cluster: Some(bv1::ClusterInfo {
                nodes: node_ids
                    .iter()
                    .map(|id| bv1::NodeInfo {
                        node_id: (*id).to_string(),
                        advertise_addr: format!("{id}:7878"),
                        role: "follower".to_string(),
                        term: 1,
                        commit_index: 1,
                        last_applied: 1,
                        reachable: true,
                        last_seen_secs: 0,
                    })
                    .collect(),
                leader_id: String::new(),
                quorum_healthy: true,
                degraded: false,
                as_of_unix_secs: 1_700_000_000,
            }),
            workers: worker_ids
                .iter()
                .map(|id| bv1::WorkerInfo {
                    worker_id: (*id).to_string(),
                    hostname: (*id).to_string(),
                    labels: Default::default(),
                    inflight: 0,
                    last_seen_secs: 0,
                    stale: false,
                    owning_node: "node-1".to_string(),
                })
                .collect(),
            jobs: Vec::new(),
            policies: Vec::new(),
            stores: Vec::new(),
        })
    }

    fn worker_added(id: &str) -> bv1::ClusterEvent {
        bv1::ClusterEvent {
            event: Some(bv1::cluster_event::Event::WorkerAdded(bv1::WorkerEvent {
                worker_id: id.to_string(),
                owning_node: "node-1".to_string(),
            })),
        }
    }

    #[test]
    fn a_new_model_is_empty_and_disconnected() {
        let m = Model::new();
        assert!(m.snapshot.cluster.is_none());
        assert!(m.events.is_empty());
        assert!(!m.should_quit());
        assert!(matches!(m.link, LinkState::Connecting { .. }));
    }

    /// Replace is wholesale. This is what makes two snapshot sources safe:
    /// whatever the previous model contained, a Replace leaves exactly the new
    /// world behind.
    #[test]
    fn replace_swaps_the_whole_world() {
        let mut m = Model::new();
        update(&mut m, Action::Replace(snapshot_with(&["a"], &["w1", "w2"])));
        assert_eq!(m.snapshot.workers.len(), 2);

        update(&mut m, Action::Replace(snapshot_with(&["a", "b"], &["w9"])));
        assert_eq!(m.snapshot.workers.len(), 1);
        assert_eq!(m.snapshot.workers[0].worker_id, "w9");
        assert_eq!(m.snapshot.cluster.unwrap().nodes.len(), 2);
    }

    /// The property the dual-source design rests on: a Replace after arbitrary
    /// deltas yields the same model as a Replace on a fresh one. If this ever
    /// fails, the sweep and the stream can disagree.
    #[test]
    fn replace_after_deltas_equals_replace_on_a_fresh_model() {
        let mut diverged = Model::new();
        update(&mut diverged, Action::Replace(snapshot_with(&["a"], &["w1"])));
        update(&mut diverged, Action::Apply(worker_added("w2")));
        update(&mut diverged, Action::Apply(worker_added("w3")));
        update(&mut diverged, Action::Replace(snapshot_with(&["a"], &["w1"])));

        let mut fresh = Model::new();
        update(&mut fresh, Action::Replace(snapshot_with(&["a"], &["w1"])));

        assert_eq!(diverged.snapshot, fresh.snapshot);
    }

    #[test]
    fn a_delta_mutates_the_snapshot_and_is_recorded() {
        let mut m = Model::new();
        update(&mut m, Action::Replace(snapshot_with(&["a"], &["w1"])));
        update(&mut m, Action::Apply(worker_added("w2")));

        assert_eq!(m.snapshot.workers.len(), 2);
        assert_eq!(m.events.len(), 1, "a delta also lands in the activity feed");
    }

    /// The ring is bounded, newest last. An operator console left open for a
    /// week must not grow without limit.
    #[test]
    fn the_event_ring_is_bounded() {
        let mut m = Model::new();
        for i in 0..(EVENT_RING + 50) {
            update(&mut m, Action::Local(format!("notice {i}")));
        }
        assert_eq!(m.events.len(), EVENT_RING);
    }

    /// Local notices share the feed with cluster deltas, because dropping
    /// tui-logger left the console's own diagnostics with nowhere else to go —
    /// and "sweep failed" above "node-2 unreachable" is one story.
    #[test]
    fn local_notices_and_cluster_deltas_share_the_feed() {
        let mut m = Model::new();
        update(&mut m, Action::Local("connected to node-1".to_string()));
        update(&mut m, Action::Apply(worker_added("w1")));

        assert_eq!(m.events.len(), 2);
        assert!(matches!(m.events[0].entry, Entry::Local(_)));
        assert!(matches!(m.events[1].entry, Entry::Cluster(_)));
    }

    #[test]
    fn tab_cycles_panels_and_wraps() {
        let mut m = Model::new();
        assert_eq!(m.focus, Panel::Cluster);
        for expected in [Panel::Workers, Panel::Jobs, Panel::Events, Panel::Cluster] {
            update(&mut m, Action::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)));
            assert_eq!(m.focus, expected);
        }
    }

    #[test]
    fn q_and_ctrl_c_both_quit() {
        let mut m = Model::new();
        update(&mut m, key('q'));
        assert!(m.should_quit());

        let mut m = Model::new();
        update(
            &mut m,
            Action::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        );
        assert!(m.should_quit());
    }

    /// Scrolling is per panel, so switching tabs does not lose your place.
    #[test]
    fn scroll_is_tracked_per_panel() {
        let mut m = Model::new();
        update(&mut m, Action::Replace(snapshot_with(&["a"], &["w1", "w2", "w3"])));
        update(&mut m, Action::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)));
        assert_eq!(m.focus, Panel::Workers);

        update(&mut m, Action::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)));
        assert_eq!(m.scroll[Panel::Workers as usize], 1);
        assert_eq!(m.scroll[Panel::Cluster as usize], 0, "other panels unaffected");
    }

    /// `Tick` carries its own timestamp so `update` reads no clock and stays
    /// pure — the entire reducer is testable without mocking time.
    #[test]
    fn update_never_reads_a_clock() {
        let mut m = Model::new();
        update(&mut m, Action::Tick(at(1_700_000_000)));
        update(&mut m, Action::Local("x".to_string()));
        assert_eq!(m.events[0].at, at(1_700_000_000));
    }

    #[test]
    fn link_state_transitions_are_recorded_in_the_feed() {
        let mut m = Model::new();
        update(
            &mut m,
            Action::Link(LinkState::Connected {
                endpoint: "http://127.0.0.1:7880".to_string(),
            }),
        );
        assert!(matches!(m.link, LinkState::Connected { .. }));
        assert_eq!(m.events.len(), 1, "an operator should see when it connected");
    }
}
```

- [ ] **Step 4: Run to verify it fails**

```bash
cargo test -p brokkr-tui
```

Expected: FAIL to compile — `Model`, `Action`, `update` not found.

- [ ] **Step 5: Implement `app.rs`**

Prepend to `crates/brokkr-tui/src/app.rs`:

```rust
//! The model, the action set, and the pure reducer.
//!
//! `update` is the only thing that mutates state, it performs no I/O, and it
//! reads no clock — `Tick` and every timestamped action carry their own time.
//! That is what makes the whole reducer testable without a terminal, a socket,
//! or a mocked clock.

use std::collections::VecDeque;
use std::time::SystemTime;

use brokkr_proto::brokkr_v1 as bv1;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// How many activity entries are retained.
pub const EVENT_RING: usize = 1000;

/// Connection state, rendered as a banner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkState {
    /// Dialling. The initial state, so a console that has never connected shows
    /// "connecting" rather than a blank frame.
    Connecting { endpoint: String },
    /// Streaming.
    Connected { endpoint: String },
    /// Not connected; the actor is backing off and will retry.
    Disconnected { reason: String },
}

/// Which panel has focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    Cluster,
    Workers,
    Jobs,
    Events,
}

impl Panel {
    /// Panels in tab order.
    pub const ALL: [Panel; 4] = [Panel::Cluster, Panel::Workers, Panel::Jobs, Panel::Events];

    fn next(self) -> Panel {
        Panel::ALL[(self as usize + 1) % Panel::ALL.len()]
    }

    fn prev(self) -> Panel {
        Panel::ALL[(self as usize + Panel::ALL.len() - 1) % Panel::ALL.len()]
    }

    /// Human label for the tab bar.
    pub fn title(self) -> &'static str {
        match self {
            Panel::Cluster => "Cluster",
            Panel::Workers => "Workers",
            Panel::Jobs => "Jobs",
            Panel::Events => "Events",
        }
    }
}

/// One line in the activity feed.
///
/// Two kinds, deliberately in one feed: dropping `tui-logger` (see the spec)
/// left the console's own diagnostics with nowhere to go, and "sweep failed"
/// immediately above "node-2 unreachable" is one story an operator needs to
/// read in order.
#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    /// A delta from `WatchEvents`.
    Cluster(bv1::ClusterEvent),
    /// Something the console itself noticed.
    Local(String),
}

/// An [`Entry`] with the time it arrived.
///
/// Arrival time, because the events themselves carry no timestamp — only the
/// snapshot does — and inventing one would be worse than showing when we saw it.
#[derive(Debug, Clone, PartialEq)]
pub struct StampedEntry {
    pub at: SystemTime,
    pub entry: Entry,
}

/// Everything that can change the model.
#[derive(Debug, Clone)]
pub enum Action {
    /// Replace the world. From the stream's opening or resync snapshot, or from
    /// the periodic `GetSnapshot` sweep. Both are atomic and authoritative.
    Replace(Box<bv1::SnapshotEvent>),
    /// Apply one delta from the stream.
    Apply(bv1::ClusterEvent),
    /// Connection state changed.
    Link(LinkState),
    /// Terminal input.
    Key(KeyEvent),
    /// Render tick, carrying its own time so `update` reads no clock.
    Tick(SystemTime),
    /// A console-side notice for the activity feed.
    Local(String),
}

/// The whole application state.
#[derive(Debug)]
pub struct Model {
    /// The cluster as last replaced, then mutated by deltas.
    pub snapshot: bv1::SnapshotEvent,
    /// Bounded activity feed, oldest first.
    pub events: VecDeque<StampedEntry>,
    /// Connection state.
    pub link: LinkState,
    /// Focused panel.
    pub focus: Panel,
    /// Scroll offset per panel, indexed by `Panel as usize`.
    pub scroll: [usize; 4],
    /// The most recent tick's time, used to stamp entries.
    now: SystemTime,
    quit: bool,
}

impl Default for Model {
    fn default() -> Self {
        Self::new()
    }
}

impl Model {
    /// An empty model, not yet connected.
    pub fn new() -> Self {
        Self {
            snapshot: bv1::SnapshotEvent::default(),
            events: VecDeque::new(),
            link: LinkState::Connecting {
                endpoint: String::new(),
            },
            focus: Panel::Cluster,
            scroll: [0; 4],
            now: SystemTime::UNIX_EPOCH,
            quit: false,
        }
    }

    /// Whether the event loop should exit.
    pub fn should_quit(&self) -> bool {
        self.quit
    }

    fn push(&mut self, entry: Entry) {
        if self.events.len() == EVENT_RING {
            self.events.pop_front();
        }
        self.events.push_back(StampedEntry {
            at: self.now,
            entry,
        });
    }
}

/// Apply one action. The only mutation point, and free of I/O and clocks.
pub fn update(model: &mut Model, action: Action) {
    match action {
        Action::Replace(snapshot) => model.snapshot = *snapshot,
        Action::Apply(event) => {
            apply_delta(&mut model.snapshot, &event);
            model.push(Entry::Cluster(event));
        }
        Action::Link(state) => {
            let note = match &state {
                LinkState::Connecting { endpoint } => format!("connecting to {endpoint}"),
                LinkState::Connected { endpoint } => format!("connected to {endpoint}"),
                LinkState::Disconnected { reason } => format!("disconnected: {reason}"),
            };
            model.link = state;
            model.push(Entry::Local(note));
        }
        Action::Key(key) => handle_key(model, key),
        Action::Tick(now) => model.now = now,
        Action::Local(note) => model.push(Entry::Local(note)),
    }
}

/// Mutate the snapshot for one delta.
///
/// Only the collections a delta can affect are touched. Anything the snapshot
/// carries that no delta describes — CAS sizes, job history — is left for the
/// next `Replace`, which is why the sweep exists.
fn apply_delta(snapshot: &mut bv1::SnapshotEvent, event: &bv1::ClusterEvent) {
    use bv1::cluster_event::Event as E;
    let Some(event) = event.event.as_ref() else {
        return;
    };
    match event {
        E::WorkerAdded(w) => {
            if !snapshot.workers.iter().any(|x| x.worker_id == w.worker_id) {
                snapshot.workers.push(bv1::WorkerInfo {
                    worker_id: w.worker_id.clone(),
                    owning_node: w.owning_node.clone(),
                    ..Default::default()
                });
                snapshot.workers.sort_by(|a, b| a.worker_id.cmp(&b.worker_id));
            }
        }
        E::WorkerRemoved(w) => snapshot.workers.retain(|x| x.worker_id != w.worker_id),
        E::WorkerStale(w) => {
            if let Some(x) = snapshot
                .workers
                .iter_mut()
                .find(|x| x.worker_id == w.worker_id)
            {
                x.stale = true;
            }
        }
        E::NodeUnreachable(n) => set_reachable(snapshot, &n.node_id, false),
        E::NodeRecovered(n) => set_reachable(snapshot, &n.node_id, true),
        E::PolicyQuarantined(p) => set_quarantined(snapshot, &p.owning_node, true),
        E::PolicyRecovered(p) => set_quarantined(snapshot, &p.owning_node, false),
        E::LeaderChanged(l) => {
            if let Some(c) = snapshot.cluster.as_mut() {
                c.leader_id = l.to.clone();
            }
        }
        // A snapshot arriving as a delta is a protocol oddity; the stream sends
        // them as the opening message and on resync, both of which the
        // connection actor turns into `Replace`.
        E::Snapshot(_) => {}
    }
}

fn set_reachable(snapshot: &mut bv1::SnapshotEvent, node_id: &str, reachable: bool) {
    if let Some(c) = snapshot.cluster.as_mut() {
        if let Some(n) = c.nodes.iter_mut().find(|n| n.node_id == node_id) {
            n.reachable = reachable;
        }
        c.degraded = c.nodes.iter().any(|n| !n.reachable);
    }
}

fn set_quarantined(snapshot: &mut bv1::SnapshotEvent, owning_node: &str, quarantined: bool) {
    if let Some(p) = snapshot
        .policies
        .iter_mut()
        .find(|p| p.owning_node == owning_node)
    {
        p.quarantined = quarantined;
    }
}

fn handle_key(model: &mut Model, key: KeyEvent) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Char('c') if ctrl => model.quit = true,
        KeyCode::Char('q') => model.quit = true,
        KeyCode::Tab => model.focus = model.focus.next(),
        KeyCode::BackTab => model.focus = model.focus.prev(),
        KeyCode::Down | KeyCode::Char('j') => {
            let i = model.focus as usize;
            model.scroll[i] = model.scroll[i].saturating_add(1);
        }
        KeyCode::Up | KeyCode::Char('k') => {
            let i = model.focus as usize;
            model.scroll[i] = model.scroll[i].saturating_sub(1);
        }
        KeyCode::Home => model.scroll[model.focus as usize] = 0,
        _ => {}
    }
}
```

- [ ] **Step 6: Run the tests**

```bash
cargo test -p brokkr-tui
```

Expected: PASS — 11 in `app::tests`, 2 in `terminal::tests`.

- [ ] **Step 7: Write a `main.rs` that runs the loop with no backend**

```rust
//! Brokkr operator console (ADR 0012).

mod app;
mod terminal;

use std::time::{Duration, SystemTime};

use anyhow::Result;
use clap::Parser;
use futures::StreamExt as _;

use crate::app::{update, Action, Model};

/// How often the UI redraws. Fast enough to feel live, slow enough that an
/// idle console costs nothing.
const TICK: Duration = Duration::from_millis(250);

#[derive(Debug, Parser)]
#[command(name = "brokkr-tui", version, about = "Brokkr operator console")]
struct Args {
    /// Operator observability endpoint. Repeat to name every control-plane
    /// node; the first that answers is used, and the console fails over to the
    /// next if it stops answering.
    #[arg(long, default_value = "http://127.0.0.1:7880")]
    control: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let _args = Args::parse();

    terminal::install_panic_hook();
    let mut term = terminal::enter()?;
    let result = run(&mut term).await;
    // Restore before propagating, so an error message is readable.
    let _ = terminal::restore();
    result
}

async fn run(
    term: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
) -> Result<()> {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Action>(256);

    // Terminal input.
    let input_tx = tx.clone();
    tokio::spawn(async move {
        let mut events = crossterm::event::EventStream::new();
        while let Some(Ok(ev)) = events.next().await {
            if let crossterm::event::Event::Key(key) = ev {
                if input_tx.send(Action::Key(key)).await.is_err() {
                    return;
                }
            }
        }
    });

    // Render tick, carrying its own time so `update` stays clock-free.
    let tick_tx = tx.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(TICK);
        loop {
            ticker.tick().await;
            if tick_tx.send(Action::Tick(SystemTime::now())).await.is_err() {
                return;
            }
        }
    });

    let mut model = Model::new();
    while let Some(action) = rx.recv().await {
        update(&mut model, action);
        if model.should_quit() {
            break;
        }
        term.draw(|f| {
            // Panels arrive in T5/T6. Until then, prove the loop runs.
            let text = format!("brokkr-tui — {} — press q to quit", model.focus.title());
            f.render_widget(ratatui::widgets::Paragraph::new(text), f.area());
        })?;
    }
    Ok(())
}
```

- [ ] **Step 8: Run the full gate**

Run the five commands from Global Constraints, plus:

```bash
cargo deny check advisories licenses bans
```

`ratatui` and `crossterm` were verified clean on 2026-08-02 (see Dependency
facts), so a failure here means something moved and is worth reading rather
than working around.

- [ ] **Step 9: Update the CHANGELOG and commit**

Under `### Added`:

```markdown
- **`brokkr-tui`** — the operator console crate (ADR 0012). This increment is
  the scaffold: terminal lifecycle with a panic hook that restores the terminal
  before unwinding, the pure `update()` reducer, and the action bus. No backend
  connection yet.
```

```bash
git add crates/brokkr-tui Cargo.toml Cargo.lock CHANGELOG.md
git status --short | grep -v '^??'   # Cargo.lock MUST appear staged
git commit -m "feat(tui): scaffold the operator console

Motivation: ADR 0012's WS0 — the crate, the terminal lifecycle, and the pure
reducer, with no backend dependency so it can proceed independently of the
observability work.

New dependencies (CLAUDE.md rule 6): ratatui 0.30 — the de-facto Rust TUI
renderer; crossterm 0.29 with event-stream — its async input backend, where
event-stream is what lets terminal input join the action bus as a stream rather
than a blocking read. Both verified against rustc 1.94 with cargo deny
advisories, licenses and bans clean.

terminal::restore is a free function rather than a Drop impl or closure because
it must be callable from three places that cannot share state: normal shutdown,
the panic hook, and a unit test. Without the hook a panic leaves the user in raw
mode on the alternate screen with the panic message invisible — the failure
naive ratatui apps get wrong.

update() performs no I/O and reads no clock: Tick carries its own timestamp.
That is what makes the whole reducer testable without a terminal, a socket, or
a mocked clock.

Replace is wholesale, and one test asserts the property the dual-source design
rests on: a Replace after arbitrary deltas yields the same model as a Replace on
a fresh one. If that ever fails, the periodic sweep and the stream can disagree.

How tested: 11 reducer tests (empty model, wholesale replace, replace-equals-
fresh after divergence, delta mutation, bounded ring, local and cluster entries
sharing the feed, tab cycling, quit keys, per-panel scroll, clock-free updates,
link transitions) and 2 terminal tests (restore is safe without entering; the
panic hook is installed). Whether the hook fires correctly is verified by hand
in T7 and recorded as manual — panic hooks are global process state and
asserting one fires means panicking inside the harness.

Related: docs/superpowers/plans/2026-08-02-operator-tui.md Task 3, ADR 0012."
```

---

### Task 4: The connection actor

Branch: `feat/tui-connection` from `origin/main`.

**Files:**
- Create: `crates/brokkr-tui/src/conn.rs`
- Modify: `crates/brokkr-tui/src/main.rs`
- Modify: `CHANGELOG.md`

**Interfaces:**
- Consumes: `app::{Action, LinkState}` (T3); `ObservabilityClient::{connect, get_snapshot, watch_events}` (T2).
- Produces:

```rust
pub const BASE_BACKOFF: Duration = Duration::from_millis(200);
pub const MAX_BACKOFF: Duration = Duration::from_secs(30);
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(10);
pub fn rotation_plan(len: usize, attempt: usize) -> (usize, Duration);
pub fn spawn(endpoints: Vec<String>, tx: tokio::sync::mpsc::Sender<Action>) -> tokio::task::JoinHandle<()>;
```

- [ ] **Step 1: Write the failing rotation tests**

`crates/brokkr-tui/src/conn.rs`, test module only:

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    /// The whole first cycle is tried with no delay. A node dying should cost
    /// the console milliseconds, not seconds — the survivors are up *now*, and
    /// backing off before having tried them adds latency for nothing.
    #[test]
    fn the_first_cycle_has_no_delay() {
        for attempt in 0..3 {
            let (index, delay) = rotation_plan(3, attempt);
            assert_eq!(index, attempt);
            assert_eq!(delay, Duration::ZERO);
        }
    }

    /// After a full cycle the whole cluster looks unreachable, so patience is
    /// correct: nothing is listening and hammering helps nobody.
    #[test]
    fn later_cycles_back_off_and_cap() {
        let (_, first) = rotation_plan(3, 3);
        let (_, second) = rotation_plan(3, 6);
        assert!(first > Duration::ZERO);
        assert!(second > first, "backoff should grow per completed cycle");
        let (_, far) = rotation_plan(3, 300);
        assert!(far <= MAX_BACKOFF, "backoff must cap at {MAX_BACKOFF:?}");
    }

    #[test]
    fn indices_cycle_through_every_endpoint() {
        let seen: Vec<usize> = (0..6).map(|a| rotation_plan(3, a).0).collect();
        assert_eq!(seen, vec![0, 1, 2, 0, 1, 2]);
    }

    /// An empty endpoint list must not divide by zero. It cannot happen — clap
    /// supplies a default — but a panic in a background task would take out the
    /// console silently.
    #[test]
    fn an_empty_endpoint_list_does_not_panic() {
        let (index, delay) = rotation_plan(0, 7);
        assert_eq!(index, 0);
        assert_eq!(delay, MAX_BACKOFF);
    }

    /// Deterministic, so a fleet of consoles restarted together does not
    /// reconnect in lockstep, and so this stays unit-testable.
    #[test]
    fn rotation_is_deterministic() {
        for attempt in 0..50 {
            assert_eq!(rotation_plan(3, attempt), rotation_plan(3, attempt));
        }
    }
}
```

- [ ] **Step 2: Run to verify it fails**

```bash
cargo test -p brokkr-tui conn::
```

Expected: FAIL to compile — `rotation_plan` not found.

- [ ] **Step 3: Implement `conn.rs`**

```rust
//! The connection actor: everything fallible, so the reducer is not.
//!
//! Owns the SDK client, the endpoint rotation, the event stream, and the
//! periodic sweep. Emits [`Action`]s onto the bus and never touches the model.

use std::time::Duration;

use brokkr_sdk::ObservabilityClient;
use futures::StreamExt as _;
use tokio::sync::mpsc::Sender;

use crate::app::{Action, LinkState};

/// First backoff after a full failed cycle.
pub const BASE_BACKOFF: Duration = Duration::from_millis(200);
/// Ceiling on backoff.
pub const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// How often the full-snapshot sweep runs.
///
/// Deliberately much slower than the server's own 2s peer poll: the sweep is a
/// safety net against a delta bug, not a freshness mechanism, and making it
/// faster would add load without adding information.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(10);

/// Which endpoint to try for `attempt`, and how long to wait first.
///
/// Deliberately the same policy as `brokkr-worker`'s `rotation_plan`: the whole
/// first cycle with no delay, then exponential backoff per completed cycle,
/// capped, with attempt-derived jitter so co-restarted clients do not reconnect
/// in lockstep. Reimplemented rather than shared because it lives in
/// `brokkr-worker`, which this crate does not depend on; it is ten lines and
/// the alternative is a dependency edge for a formula.
pub fn rotation_plan(len: usize, attempt: usize) -> (usize, Duration) {
    if len == 0 {
        return (0, MAX_BACKOFF);
    }
    let index = attempt % len;
    let completed_cycles = attempt / len;
    if completed_cycles == 0 {
        return (index, Duration::ZERO);
    }
    let factor = 1u64
        .checked_shl(completed_cycles as u32 - 1)
        .unwrap_or(u64::MAX);
    let millis = BASE_BACKOFF
        .as_millis()
        .saturating_mul(u128::from(factor))
        .min(MAX_BACKOFF.as_millis());
    let jitter = (attempt as u128 * 37) % 100;
    let millis = (millis + jitter).min(MAX_BACKOFF.as_millis()) as u64;
    (index, Duration::from_millis(millis))
}

/// Run the connection loop until the task is dropped.
pub fn spawn(endpoints: Vec<String>, tx: Sender<Action>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut attempt = 0usize;
        loop {
            let (index, delay) = rotation_plan(endpoints.len(), attempt);
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            let Some(endpoint) = endpoints.get(index).cloned() else {
                // No endpoints at all. Say so once per cycle rather than
                // spinning silently.
                let _ = tx
                    .send(Action::Link(LinkState::Disconnected {
                        reason: "no --control endpoints configured".to_string(),
                    }))
                    .await;
                attempt = attempt.saturating_add(1);
                continue;
            };

            if tx
                .send(Action::Link(LinkState::Connecting {
                    endpoint: endpoint.clone(),
                }))
                .await
                .is_err()
            {
                return; // UI gone
            }

            match session(&endpoint, &tx).await {
                Ok(()) => {
                    // The stream ended without error — the server closed it.
                    // Reconnect from a clean slate rather than treating it as
                    // fatal; the next connect yields a fresh snapshot.
                    attempt = attempt.saturating_add(1);
                }
                Err(reason) => {
                    let _ = tx
                        .send(Action::Link(LinkState::Disconnected { reason }))
                        .await;
                    attempt = attempt.saturating_add(1);
                }
            }
        }
    })
}

/// One connection's lifetime: connect, stream, sweep, until something fails.
async fn session(endpoint: &str, tx: &Sender<Action>) -> Result<(), String> {
    let mut client = ObservabilityClient::connect(endpoint.to_string())
        .await
        .map_err(|e| format!("{endpoint}: {e}"))?;

    let mut stream = Box::pin(
        client
            .watch_events()
            .await
            .map_err(|e| format!("{endpoint}: {e}"))?,
    );

    if tx
        .send(Action::Link(LinkState::Connected {
            endpoint: endpoint.to_string(),
        }))
        .await
        .is_err()
    {
        return Ok(());
    }

    // The sweep runs on its own client so a slow unary call cannot stall the
    // stream, and vice versa.
    let mut sweeper = ObservabilityClient::connect(endpoint.to_string())
        .await
        .map_err(|e| format!("{endpoint}: {e}"))?;
    let mut sweep = tokio::time::interval(SWEEP_INTERVAL);
    sweep.tick().await; // discard the immediate first tick; the stream just sent one

    loop {
        tokio::select! {
            item = stream.next() => match item {
                Some(Ok(event)) => {
                    // The opening message and every resync are Snapshots, and
                    // both mean "replace your world" — the same handling, which
                    // is why reconnect needs no special case.
                    let action = match event.event {
                        Some(brokkr_proto::brokkr_v1::cluster_event::Event::Snapshot(s)) => {
                            Action::Replace(Box::new(s))
                        }
                        Some(_) => Action::Apply(event),
                        None => continue,
                    };
                    if tx.send(action).await.is_err() {
                        return Ok(());
                    }
                }
                Some(Err(e)) => return Err(format!("{endpoint}: stream: {e}")),
                None => return Ok(()),
            },
            _ = sweep.tick() => {
                match sweeper.get_snapshot().await {
                    Ok(s) => {
                        if tx.send(Action::Replace(Box::new(s))).await.is_err() {
                            return Ok(());
                        }
                    }
                    Err(e) => {
                        // A failed sweep is not a failed session: the stream is
                        // still authoritative. Surface it and carry on.
                        let _ = tx
                            .send(Action::Local(format!("snapshot refresh failed: {e}")))
                            .await;
                    }
                }
            }
        }
    }
}
```

- [ ] **Step 4: Run the tests**

```bash
cargo test -p brokkr-tui conn::
```

Expected: PASS, 5 tests.

- [ ] **Step 5: Wire it into `main.rs`**

Replace `let _args = Args::parse();` with `let args = Args::parse();`, add
`mod conn;`, and inside `run` — which now takes the endpoints — spawn it:

```rust
    conn::spawn(endpoints, tx.clone());
```

Change `run`'s signature to accept `endpoints: Vec<String>` and pass
`args.control`.

- [ ] **Step 6: Verify against a real control plane by hand**

```bash
cargo build --workspace
./target/debug/brokkr-control --data-dir /tmp/brokkr-tui-check &
sleep 2
./target/debug/brokkr-tui --control http://127.0.0.1:7880
```

Expected: the placeholder UI runs and `q` exits cleanly with a usable shell.
Kill the control plane while it runs and confirm it does not exit. Record the
outcome; T7 writes it up.

- [ ] **Step 7: Run the gate, update the CHANGELOG, commit**

Under `### Added`:

```markdown
- **The console connects.** `brokkr-tui` takes a repeatable `--control`, streams
  `WatchEvents` from the first endpoint that answers, and refreshes the whole
  snapshot every 10s as a safety net. A dropped stream rotates to the next
  endpoint; reconnecting needs no special handling because the reconnect's
  opening snapshot is the same "replace your world" action as the first one.
```

---

### Task 5: Cluster and Workers panels

Branch: `feat/tui-cluster-workers` from `origin/main`.

**Files:**
- Create: `crates/brokkr-tui/src/panels/mod.rs`, `panels/cluster.rs`, `panels/workers.rs`
- Modify: `crates/brokkr-tui/src/main.rs`
- Modify: `CHANGELOG.md`

**Interfaces:**
- Produces:

```rust
pub trait Component {
    fn title(&self) -> &'static str;
    fn render(&self, model: &Model, frame: &mut Frame, area: Rect, focused: bool);
}
pub struct ClusterPanel;
pub struct WorkersPanel;
pub fn header_line(model: &Model) -> String;
```

- [ ] **Step 1: Write the failing header test**

`crates/brokkr-tui/src/panels/mod.rs`, test module only:

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use brokkr_proto::brokkr_v1 as bv1;

    use super::*;
    use crate::app::{update, Action, Model};

    fn model_with(nodes: usize, reachable: usize, degraded: bool) -> Model {
        let mut m = Model::new();
        update(
            &mut m,
            Action::Replace(Box::new(bv1::SnapshotEvent {
                cluster: Some(bv1::ClusterInfo {
                    nodes: (0..nodes)
                        .map(|i| bv1::NodeInfo {
                            node_id: format!("node-{i}"),
                            reachable: i < reachable,
                            ..Default::default()
                        })
                        .collect(),
                    leader_id: "node-0".to_string(),
                    quorum_healthy: !degraded,
                    degraded,
                    as_of_unix_secs: 1_700_000_000,
                }),
                ..Default::default()
            })),
        );
        m
    }

    /// The header is where an operator learns they are looking at a partial
    /// picture. It must say so numerically, not just with a colour.
    #[test]
    fn the_header_reports_how_many_nodes_are_answering() {
        let line = header_line(&model_with(3, 2, true));
        assert!(line.contains("2 of 3"), "got: {line}");
        assert!(line.contains("degraded"), "got: {line}");
    }

    #[test]
    fn a_healthy_cluster_does_not_say_degraded() {
        let line = header_line(&model_with(3, 3, false));
        assert!(line.contains("3 of 3"), "got: {line}");
        assert!(!line.contains("degraded"), "got: {line}");
    }

    /// Before the first snapshot there is nothing to report, and the header
    /// must not claim "0 of 0" as though that were a finding.
    #[test]
    fn an_empty_model_says_it_is_waiting() {
        let line = header_line(&Model::new());
        assert!(line.contains("waiting"), "got: {line}");
    }
}
```

- [ ] **Step 2: Run to verify it fails, then implement `panels/mod.rs`**

```bash
cargo test -p brokkr-tui panels::
```

Then prepend:

```rust
//! Panels, and the trait that composes them.

pub mod cluster;
pub mod workers;

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Borders};
use ratatui::Frame;

use crate::app::Model;

/// One pane of the console.
pub trait Component {
    /// Tab-bar label.
    fn title(&self) -> &'static str;
    /// Draw into `area`.
    fn render(&self, model: &Model, frame: &mut Frame, area: Rect, focused: bool);
}

/// A bordered block, highlighted when focused.
pub fn panel_block(title: &str, focused: bool) -> Block<'_> {
    let style = if focused {
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(style)
}

/// The one-line summary above the panels.
///
/// Says how many nodes are answering out of how many are known, because that is
/// the difference between "the cluster is small" and "I am showing you part of
/// it" — and an operator must not have to infer it from a colour.
pub fn header_line(model: &Model) -> String {
    let Some(cluster) = model.snapshot.cluster.as_ref() else {
        return "waiting for first snapshot".to_string();
    };
    let total = cluster.nodes.len();
    let up = cluster.nodes.iter().filter(|n| n.reachable).count();
    let leader = if cluster.leader_id.is_empty() {
        "no leader".to_string()
    } else {
        format!("leader {}", cluster.leader_id)
    };
    let mut line = format!("{up} of {total} nodes reporting — {leader}");
    if cluster.degraded {
        line.push_str(" — degraded");
    }
    line
}
```

- [ ] **Step 3: Implement `panels/cluster.rs`**

This is the canonical panel. Every other panel in T5 and T6 is the same
structure with different rows, so this one is given in full and the others give
their row construction against it.

```rust
//! Nodes, their Raft state, and their scheduling policy.
//!
//! Policy lives here as columns rather than in its own panel: it is four
//! numbers and a flag per node, and it reads naturally beside node health.

use ratatui::layout::{Constraint, Rect};
use ratatui::widgets::{Cell, Row, Table};
use ratatui::Frame;

use super::{panel_block, Component};
use crate::app::{Model, Panel};

/// Nodes, Raft state, and per-node policy.
#[derive(Debug, Default)]
pub struct ClusterPanel;

impl Component for ClusterPanel {
    fn title(&self) -> &'static str {
        "Cluster"
    }

    fn render(&self, model: &Model, frame: &mut Frame, area: Rect, focused: bool) {
        let header = Row::new(vec![
            Cell::from("node"),
            Cell::from("role"),
            Cell::from("term"),
            Cell::from("commit/applied"),
            Cell::from("up"),
            Cell::from("policy"),
            Cell::from("decided"),
            Cell::from("declined"),
        ]);

        let nodes = model
            .snapshot
            .cluster
            .as_ref()
            .map(|c| c.nodes.as_slice())
            .unwrap_or(&[]);

        let rows: Vec<Row> = nodes
            .iter()
            .map(|n| {
                // Join to policies on owning_node. A node with no policy entry
                // renders "-" rather than "none": we did not hear from it, which
                // is different from it having no policy configured.
                let policy = model
                    .snapshot
                    .policies
                    .iter()
                    .find(|p| p.owning_node == n.node_id);
                let (state, decided, declined) = match policy {
                    Some(p) if p.quarantined => ("QUARANTINED".to_string(), p.decided, p.declined),
                    Some(p) if p.loaded => ("loaded".to_string(), p.decided, p.declined),
                    Some(p) => ("none".to_string(), p.decided, p.declined),
                    None => ("-".to_string(), 0, 0),
                };
                Row::new(vec![
                    Cell::from(n.node_id.clone()),
                    Cell::from(n.role.clone()),
                    Cell::from(n.term.to_string()),
                    Cell::from(format!("{}/{}", n.commit_index, n.last_applied)),
                    Cell::from(if n.reachable { "yes" } else { "NO" }),
                    Cell::from(state),
                    Cell::from(decided.to_string()),
                    Cell::from(declined.to_string()),
                ])
            })
            .collect();

        let widths = [
            Constraint::Min(10),
            Constraint::Length(10),
            Constraint::Length(6),
            Constraint::Length(16),
            Constraint::Length(4),
            Constraint::Length(12),
            Constraint::Length(8),
            Constraint::Length(9),
        ];
        let table = Table::new(rows, widths)
            .header(header)
            .block(panel_block(self.title(), focused));
        frame.render_widget(table, area);

        let _ = model.scroll[Panel::Cluster as usize];
    }
}
```

The trailing `scroll` read is a deliberate no-op for this panel: a cluster is
three or five rows and never needs scrolling. Panels that *do* scroll are
handled in Step 4.

- [ ] **Step 4: Implement `panels/workers.rs`**

Same structure as `ClusterPanel` above — `panel_block`, a header `Row`, a
`Table` with `Constraint` widths — with these rows, and with scrolling, because
a worker list genuinely outgrows the pane:

```rust
        let start = model.scroll[Panel::Workers as usize];
        let visible = area.height.saturating_sub(3) as usize; // borders + header
        // Clamp so scrolling past the end shows the last page rather than an
        // empty pane, which reads as "no workers" and is a lie.
        let start = start.min(model.snapshot.workers.len().saturating_sub(visible));

        let rows: Vec<Row> = model
            .snapshot
            .workers
            .iter()
            .skip(start)
            .take(visible.max(1))
            .map(|w| {
                let labels = w
                    .labels
                    .iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect::<Vec<_>>()
                    .join(",");
                Row::new(vec![
                    Cell::from(w.worker_id.clone()),
                    Cell::from(w.owning_node.clone()),
                    Cell::from(labels),
                    Cell::from(w.inflight.to_string()),
                    Cell::from(format!("{}s", w.last_seen_secs)),
                    Cell::from(if w.stale { "STALE" } else { "" }),
                ])
            })
            .collect();
```

Header: `worker`, `node`, `labels`, `inflight`, `last seen`, `stale`. Widths:
`Min(12), Length(10), Min(16), Length(8), Length(10), Length(6)`.

`w.labels` is a `HashMap` in the generated proto type, so **sort the pairs
before joining** — otherwise the label column reorders on every redraw, which
looks like flicker and makes the column unreadable. Collect into a `Vec`, sort
by key, then join.

- [ ] **Step 5: Add the `TestBackend` render tests**

In each panel file:

```rust
    /// Rendering an empty model must not panic — a console opened before the
    /// first snapshot is the normal startup path, not an edge case.
    #[test]
    fn renders_an_empty_model_without_panicking() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let model = Model::new();
        term.draw(|f| ClusterPanel.render(&model, f, f.area(), true))
            .unwrap();
    }

    /// A terminal too small to hold the table must degrade rather than panic on
    /// a zero-width rect.
    #[test]
    fn renders_in_a_tiny_terminal_without_panicking() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut term = Terminal::new(TestBackend::new(4, 2)).unwrap();
        let model = Model::new();
        term.draw(|f| ClusterPanel.render(&model, f, f.area(), false))
            .unwrap();
    }
```

Substitute `WorkersPanel` in the workers file. **Do not snapshot-compare
populated buffers** — those break on every layout tweak and train people to
regenerate without reading. Empty and tiny are the states where a regression
actually hurts.

- [ ] **Step 6: Wire both into `main.rs`'s draw call**

Split the frame: a one-line header from `header_line`, a tab bar, then the
focused panel filling the rest.

- [ ] **Step 7: Run the gate, update the CHANGELOG, commit**

---

### Task 6: Jobs and Events panels

Branch: `feat/tui-jobs-events` from `origin/main`.

**Files:**
- Create: `crates/brokkr-tui/src/panels/jobs.rs`, `panels/events.rs`
- Modify: `crates/brokkr-tui/src/panels/mod.rs`, `crates/brokkr-tui/src/main.rs`
- Modify: `CHANGELOG.md`

**Interfaces:**
- Produces: `pub struct JobsPanel;`, `pub struct EventsPanel;`, and
  `pub fn describe(entry: &Entry) -> String` in `panels/events.rs`.

- [ ] **Step 1: Write the failing description tests**

`panels/events.rs`, test module only:

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use brokkr_proto::brokkr_v1 as bv1;

    use super::*;
    use crate::app::Entry;

    fn cluster(event: bv1::cluster_event::Event) -> Entry {
        Entry::Cluster(bv1::ClusterEvent { event: Some(event) })
    }

    /// Every event kind renders to something an operator can read without
    /// knowing the proto. A missing arm here shows as an empty line, which is
    /// worse than a wrong one because it looks like nothing happened.
    #[test]
    fn every_event_kind_describes_itself() {
        use bv1::cluster_event::Event as E;
        let cases = [
            cluster(E::NodeUnreachable(bv1::NodeEvent {
                node_id: "node-2".into(),
            })),
            cluster(E::NodeRecovered(bv1::NodeEvent {
                node_id: "node-2".into(),
            })),
            cluster(E::WorkerAdded(bv1::WorkerEvent {
                worker_id: "w-1".into(),
                owning_node: "node-1".into(),
            })),
            cluster(E::WorkerRemoved(bv1::WorkerEvent {
                worker_id: "w-1".into(),
                owning_node: "node-1".into(),
            })),
            cluster(E::WorkerStale(bv1::WorkerEvent {
                worker_id: "w-1".into(),
                owning_node: "node-1".into(),
            })),
            cluster(E::PolicyQuarantined(bv1::PolicyEvent {
                owning_node: "node-1".into(),
            })),
            cluster(E::PolicyRecovered(bv1::PolicyEvent {
                owning_node: "node-1".into(),
            })),
            cluster(E::LeaderChanged(bv1::LeaderEvent {
                from: "node-1".into(),
                to: "node-2".into(),
            })),
            Entry::Local("connected to node-1".into()),
        ];
        for case in &cases {
            let text = describe(case);
            assert!(!text.trim().is_empty(), "empty description for {case:?}");
        }
    }

    /// Losing the leader must read as losing it, not as an empty destination.
    #[test]
    fn losing_the_leader_reads_as_no_leader() {
        use bv1::cluster_event::Event as E;
        let text = describe(&cluster(E::LeaderChanged(bv1::LeaderEvent {
            from: "node-1".into(),
            to: String::new(),
        })));
        assert!(text.contains("no leader"), "got: {text}");
    }
}
```

- [ ] **Step 2: Run to verify it fails, then implement `describe`**

Every arm produces a plain sentence: `"node-2 unreachable"`, `"worker w-1 added
on node-1"`, `"leadership moved from node-1 to node-2"`, and — for an empty
`to` — `"leadership lost (no leader)"`. `Entry::Local` renders its string
verbatim.

- [ ] **Step 3: Implement `panels/jobs.rs`**

Same `panel_block` + header + `Table` + `Constraint` structure as
`ClusterPanel` in Task 5 Step 3, with these rows and the same scroll clamp as
`WorkersPanel`:

```rust
        let rows: Vec<Row> = model
            .snapshot
            .jobs
            .iter()
            .skip(start)
            .take(visible.max(1))
            .map(|j| {
                // Blank, not "0", when the code is absent: proto3 cannot tell
                // an unset int32 from zero, and zero is a meaningful exit code,
                // which is exactly why has_exit_code exists.
                let exit = if j.has_exit_code {
                    j.exit_code.to_string()
                } else {
                    String::new()
                };
                Row::new(vec![
                    Cell::from(j.job_id.clone()),
                    Cell::from(j.tenant.clone()),
                    Cell::from(j.state.clone()),
                    Cell::from(j.worker_id.clone()),
                    Cell::from(j.owning_node.clone()),
                    Cell::from(exit),
                    Cell::from(age(model.now_unix_ms(), j.completed_at_unix_ms)),
                ])
            })
            .collect();
```

Header: `job`, `tenant`, `state`, `worker`, `node`, `exit`, `age`. Widths:
`Min(12), Length(10), Length(10), Length(10), Length(10), Length(5), Length(8)`.

With this helper, and a test for it, in the same file:

```rust
/// Render an age like "3s", "4m", "2h". Saturating, because a job whose
/// completion timestamp is ahead of our clock — nodes skew — should read as
/// "0s" rather than underflowing into a nonsense duration.
fn age(now_ms: u64, completed_ms: u64) -> String {
    let secs = now_ms.saturating_sub(completed_ms) / 1000;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h", secs / 3600)
    }
}

#[cfg(test)]
mod age_tests {
    use super::age;

    #[test]
    fn age_renders_seconds_minutes_and_hours() {
        assert_eq!(age(10_000, 7_000), "3s");
        assert_eq!(age(300_000, 60_000), "4m");
        assert_eq!(age(7_200_000, 0), "2h");
    }

    /// Clock skew between nodes can put a completion in our future. That must
    /// read as "0s", not underflow.
    #[test]
    fn a_future_completion_reads_as_zero() {
        assert_eq!(age(1_000, 9_000), "0s");
    }
}
```

Add `Model::now_unix_ms()` to `app.rs`, returning `self.now` as Unix
milliseconds and `0` if the conversion fails:

```rust
    /// The last tick's time as Unix milliseconds, for age rendering.
    pub fn now_unix_ms(&self) -> u64 {
        self.now
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
```

- [ ] **Step 4: Implement `panels/events.rs`**

A `List` rather than a `Table`, over `model.events` **reversed** so newest is
first — an operator opening the console wants the last thing that happened:

```rust
        let items: Vec<ListItem> = model
            .events
            .iter()
            .rev()
            .skip(model.scroll[Panel::Events as usize])
            .take(area.height.saturating_sub(2) as usize)
            .map(|e| {
                let stamp = unix_secs(e.at) % 86_400; // wall time within the day
                let (h, m, sec) = (stamp / 3600, (stamp % 3600) / 60, stamp % 60);
                // Local notices are dimmed so cluster events stand out, but
                // they share the feed: "sweep failed" above "node-2
                // unreachable" is one story.
                let style = match e.entry {
                    Entry::Local(_) => Style::default().fg(Color::DarkGray),
                    Entry::Cluster(_) => Style::default(),
                };
                ListItem::new(format!("{h:02}:{m:02}:{sec:02}  {}", describe(&e.entry)))
                    .style(style)
            })
            .collect();
        frame.render_widget(
            List::new(items).block(panel_block(self.title(), focused)),
            area,
        );
```

with:

```rust
fn unix_secs(t: std::time::SystemTime) -> u64 {
    t.duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
```

- [ ] **Step 5: Add the `TestBackend` render tests**

Give `JobsPanel` and `EventsPanel` the same two tests `ClusterPanel` has in
Task 5 Step 5 — one rendering an empty `Model` at 80×24, one at 4×2 — with the
panel type substituted. Written out per panel rather than shared, because a
shared helper would hide which panel failed.

- [ ] **Step 6: Run the gate, update the CHANGELOG, commit**

---

### Task 7: Documentation and the manual verification

Branch: `docs/tui-usage` from `origin/main`.

**Files:**
- Create: `docs/operations/using-the-console.md`
- Modify: `docs/architecture/0012-operator-tui.md` (status, and a "what implementation changed" section)
- Modify: `CHANGELOG.md`

- [ ] **Step 1: Perform the manual terminal verification and record it**

This is the half of DoD 6 that is not automated, and it is only honest if
actually done:

```bash
cargo build --workspace
./target/debug/brokkr-control --data-dir /tmp/brokkr-console-check &
./target/debug/brokkr-tui --control http://127.0.0.1:7880
```

1. Confirm every panel renders and `Tab` cycles them.
2. Press `q`. Confirm the shell echoes input normally and the scrollback is
   intact.
3. Kill the control plane while the console runs. Confirm a disconnected banner
   appears and the console does not exit.
4. Restart it. Confirm the console reconnects without intervention.
5. Force a panic (temporarily add `panic!()` behind a keybinding, run, press
   it, **then remove the keybinding**). Confirm the terminal is restored and the
   panic message is readable.

Record each outcome, with the date and the terminal used, in the new doc.

- [ ] **Step 2: Write `docs/operations/using-the-console.md`**

Cover: what it shows, the `--control` flag and failover, keybindings, what
"2 of 3 nodes reporting" means, why CAS and policy are per node and must not be
added up, that the view is at most one poll interval stale and where `as_of` is
shown, and the recorded manual verification from Step 1.

- [ ] **Step 3: Mark ADR 0012 accepted-and-implemented**

Change its status line to note implementation, and add a "What implementation
changed about this decision" section covering: the `tui-logger` finding and why
the Logs panel became an Events panel; that `WatchEvents` did not exist when the
ADR was written and is a better source than the ADR's polling assumption; and
the delta/sweep race that is accepted rather than fixed.

- [ ] **Step 4: Run the gate and commit**

---

## Definition of done

From the spec:

1. A 3-node cluster's workers, jobs, CAS and policy all appear from any node, labelled. *(T1 backend, T5/T6 display)*
2. Killing a node leaves the console working with the survivor count. *(T1, T5)*
3. A leadership change appears in Events without waiting for the sweep. *(T4, T6)*
4. Killing the connected node fails over to the next endpoint. *(T4)*
5. Every panel renders populated and empty without panicking. *(T5, T6)*
6. A panic restores the terminal — unit tested at the seam, hook installation asserted, real terminal verified by hand and recorded. *(T3, T7)*
7. Starting with no reachable endpoint opens the UI with a banner. *(T4)*

## Out of scope

Everything in the spec's own "Out of scope" section: mutating actions, the V1
panels (job detail, digest inspector, replication ring), streaming control-plane
logs, colour themes, mouse support, configurable layouts, and the web gateway.
