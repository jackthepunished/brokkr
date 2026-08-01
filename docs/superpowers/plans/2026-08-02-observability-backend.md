# Observability Backend Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Expose Brokkr's cluster, worker, job, CAS and scheduling-policy state over a gRPC `ObservabilityService`, aggregated across all HA nodes, on an operator-only listener.

**Architecture:** A pure `views` read-model projects node-local state into DTOs. A background poller asks every Raft peer for its local state over the peer plane (mTLS, already mutually authenticated) and merges into a `ClusterSnapshot`. Every operator-facing handler serves from that snapshot. Aggregation unions labelled per-node entities and never sums per-node measurements.

**Tech Stack:** Rust 1.94, tonic 0.12 / prost 0.13, tokio, redb, `thiserror`, `tracing`.

**Spec:** `docs/superpowers/specs/2026-08-02-observability-read-model-design.md`
**ADR:** `docs/architecture/0012-operator-tui.md`

**Scope:** This plan covers **W1–W7** (backend). The TUI (W8–W10) is a separate plan, written after this lands. The backend is independently useful — `grpcurl` against the operator listener gives you the whole cluster picture with no TUI.

## Global Constraints

Copied verbatim from CLAUDE.md and the spec. Every task's requirements implicitly include this section.

- **Never use `unwrap()` or `expect()` in library crates.** Tests and binaries may use them sparingly. Propagate with `?` and typed `thiserror` enums.
- **Never introduce `unsafe` without a `// SAFETY:` comment.**
- **Never disable a failing test to make CI green.** Fix it, or `#[ignore]` with a TODO and a tracking issue link.
- **Never add a dependency without justification** — one-line rationale in the PR description. This plan adds **no new dependencies**.
- **Never run `cargo update` as a side effect.** Lockfile changes are their own commit. A lockfile line that is a direct consequence of a dependency edge you added belongs in that PR.
- **Update `CHANGELOG.md`** under `## Unreleased` in the same commit as the change.
- **Write the test in the same commit as the implementation.**
- Rustfmt default config + `tab_spaces = 4`, `max_width = 100`. Imports grouped: std, external, local, super, self.
- Prefer `tracing::info!(field = ?value, "message")` over interpolated messages.
- Prefer `let-else` over `if let` chains for early returns.
- `#[derive(Debug)]` everywhere; **if it can't derive `Debug`, document why.**
- A file over 500 lines is a smell.
- Conventional commits. Branch `feat/<short>` or `docs/<short>` **from `origin/main`**, never from another feature branch.

**Verification before every PR — run all five, in this order, after `touch crates/*/src/lib.rs crates/*/src/main.rs`** (cargo does not re-emit diagnostics for crates it considers fresh, which has produced a false green on this project before):

```bash
touch crates/*/src/lib.rs crates/*/src/main.rs
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
cargo test --workspace --all-features --no-fail-fast > /tmp/test.log 2>&1; echo "exit=$?"
cargo deny check advisories licenses bans
```

Never pipe `cargo test` into `tail` — the pipeline reports `tail`'s status, not cargo's. Redirect and echo `$?` separately.

**Environmental test gate:** `cargo test --workspace --all-features --no-fail-fast` on the development box fails **exactly these four and nothing else**:

```
ev09_rdtsc_blocked
ev_ioctl_tiocgwinsz_blocked
ev_ioctl_tiocptlck_blocked
ev_ioctl_tiocswinsz_blocked
```

All in `crates/brokkr-sandbox/tests/evil_seccomp_caps.rs`; seccomp argument filters need a real kernel. **Any other failure is a real defect — fix it. Never weaken an assertion and never re-run hoping for green.**

**PR gate:** all six checks must be present and passing — `cargo fmt`, `cargo clippy`, `cargo doc`, `cargo deny`, `cargo test (x86_64-unknown-linux-gnu)`, `cargo test (aarch64-unknown-linux-gnu)`. Fewer than six named jobs means the workflow did not run and the green is false.

## Two traps this plan exists to avoid

Read these before Task 1. Both were found while writing the plan and both would surface as confusing bugs mid-implementation.

**1. `Instant` cannot cross the wire, and cannot be compared across nodes.**
`WorkerRecord.last_seen` is a `std::time::Instant` — monotonic, process-local, with no defined epoch. Two nodes' `Instant`s are not comparable in any way. Liveness must be converted to **"seconds since last seen", computed by the node that owns the record**, before it enters a DTO. A `NodeView.last_seen` that crossed as an absolute timestamp would be meaningless.

**2. `Cas::list_digests()` must never be on the fast poll path.**
`RedbCas::list_digests` (`crates/brokkr-cas/src/redb_backend.rs:168`) does a full table scan inside `spawn_blocking`, **holding a permit from a semaphore that returns `CasError::ThroughputLimit` when contended**. Polling it every 2s would be O(n) per poll *and* could steal a permit from real traffic. Task 2 gives CAS stats their own slow cadence.

---

## File Structure

| Path | Status | Responsibility |
|---|---|---|
| `crates/brokkr-control/src/views/mod.rs` | create | DTO definitions + re-exports |
| `crates/brokkr-control/src/views/worker.rs` | create | worker + node projections |
| `crates/brokkr-control/src/views/job.rs` | create | job summary/detail + the history ring |
| `crates/brokkr-control/src/views/policy.rs` | create | policy counter projection |
| `crates/brokkr-control/src/views/raft.rs` | create | `DriverStatus` → `NodeView` projection |
| `crates/brokkr-control/src/cluster/mod.rs` | create | `ClusterSnapshot` + poller task |
| `crates/brokkr-control/src/cluster/aggregate.rs` | create | pure merge rules (union / per-node / from-Raft) |
| `crates/brokkr-control/src/cluster/events.rs` | create | pure snapshot-diff → event list |
| `crates/brokkr-control/src/services/observability.rs` | create | `ObservabilityService` impl |
| `crates/brokkr-control/src/services/peer_observability.rs` | create | `PeerObservability` impl (no fan-out path) |
| `crates/brokkr-proto/protos/brokkr/v1/observability.proto` | create | operator-facing service + messages |
| `crates/brokkr-proto/protos/brokkr/v1/raft.proto` | modify | add `PeerObservability` service |
| `crates/brokkr-cas/src/traits.rs` | modify | add `Cas::stats()` |
| `crates/brokkr-cas/src/redb_backend.rs` | modify | override `stats()` |
| `crates/brokkr-control/src/scheduler.rs` | modify | job-history ring in `report()`; view accessors |
| `crates/brokkr-control/src/main.rs` | modify | operator listener, flags, poller wiring |
| `crates/brokkr-sdk/src/observability.rs` | create | typed read client + `watch_events()` |

`views` and `cluster` are directories rather than single files because each will
comfortably exceed 500 lines otherwise, and they split cleanly by DTO family.

---

### Task 1: `views` DTOs and node-local projections

Branch: `feat/views-read-model` from `origin/main`.

**Files:**
- Create: `crates/brokkr-control/src/views/mod.rs`
- Create: `crates/brokkr-control/src/views/worker.rs`
- Create: `crates/brokkr-control/src/views/policy.rs`
- Modify: `crates/brokkr-control/src/lib.rs` (add `pub mod views;` in alphabetical position, after `pub mod services;`)
- Modify: `CHANGELOG.md`

**Interfaces:**
- Consumes: `crate::registry::{WorkerRegistry, WorkerRecord, WorkerCapabilities, HeartbeatPolicy}`; `crate::wasm_strategy::{WasmStrategy, PolicyFailureCounts}`; `brokkr_common::WorkerId`.
- Produces, relied on by Tasks 3–6:
  - `views::WorkerView { worker_id: String, hostname: String, labels: BTreeMap<String, String>, inflight: u32, last_seen_secs: u64, stale: bool, owning_node: String }`
  - `views::PolicyView { loaded: bool, quarantined: bool, decided: u64, declined: u64, failures_by_reason: BTreeMap<String, u64>, owning_node: String }`
  - `views::worker_views(registry: &WorkerRegistry, now: Instant, owning_node: &str, inflight: &dyn Fn(&WorkerId) -> usize) -> Vec<WorkerView>`
  - `views::policy_view(strategy: Option<&WasmStrategy>, owning_node: &str) -> PolicyView`

- [ ] **Step 1: Write the failing test for worker projection**

Create `crates/brokkr-control/src/views/worker.rs` with only the test module for now:

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::disallowed_methods, clippy::panic)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::{Duration, Instant};

    use brokkr_common::WorkerId;

    use super::*;
    use crate::registry::{WorkerCapabilities, WorkerRegistry};

    fn wid(s: &str) -> WorkerId {
        WorkerId::new(s.to_string()).unwrap()
    }

    /// `Instant` is monotonic and process-local, so it cannot cross the wire
    /// and two nodes' values are not comparable. Liveness must be converted to
    /// "seconds ago" by the node that owns the record.
    #[test]
    fn liveness_crosses_as_seconds_ago_not_as_an_instant() {
        let mut reg = WorkerRegistry::default();
        let t0 = Instant::now();
        reg.register(
            wid("w-a"),
            WorkerCapabilities {
                hostname: "host-a".to_string(),
                labels: BTreeMap::from([("os".to_string(), "linux".to_string())]),
            },
            t0,
        );

        let views = worker_views(&reg, t0 + Duration::from_secs(7), "node-1", &|_| 3);

        assert_eq!(views.len(), 1);
        let v = &views[0];
        assert_eq!(v.worker_id, "w-a");
        assert_eq!(v.hostname, "host-a");
        assert_eq!(v.last_seen_secs, 7);
        assert_eq!(v.inflight, 3);
        assert_eq!(v.owning_node, "node-1");
        assert!(!v.stale);
    }

    /// Every DTO sourced from node-local state carries the node that owns it,
    /// so aggregation can never present it as a single cluster fact.
    #[test]
    fn every_worker_carries_its_owning_node() {
        let mut reg = WorkerRegistry::default();
        let t0 = Instant::now();
        for id in ["w-a", "w-b"] {
            reg.register(
                wid(id),
                WorkerCapabilities {
                    hostname: id.to_string(),
                    labels: BTreeMap::new(),
                },
                t0,
            );
        }
        let views = worker_views(&reg, t0, "node-2", &|_| 0);
        assert_eq!(views.len(), 2);
        assert!(views.iter().all(|v| v.owning_node == "node-2"));
    }

    /// Output order must not depend on `HashMap` iteration order. This project
    /// has shipped that bug twice (#174, and the Phase 6 candidate ordering).
    #[test]
    fn worker_views_are_sorted_by_id() {
        let mut reg = WorkerRegistry::default();
        let t0 = Instant::now();
        for id in ["w-zulu", "w-alpha", "w-mike"] {
            reg.register(
                wid(id),
                WorkerCapabilities {
                    hostname: id.to_string(),
                    labels: BTreeMap::new(),
                },
                t0,
            );
        }
        let ids: Vec<&str> = worker_views(&reg, t0, "n", &|_| 0)
            .iter()
            .map(|v| v.worker_id.as_str())
            .collect();
        assert_eq!(ids, vec!["w-alpha", "w-mike", "w-zulu"]);
    }

    /// A `now` earlier than `last_seen` (clock skew in a caller-supplied
    /// instant) must read as zero, not panic — the same posture
    /// `WorkerRecord::is_stale` already takes.
    #[test]
    fn a_now_before_last_seen_reads_as_zero_seconds() {
        let mut reg = WorkerRegistry::default();
        let t0 = Instant::now() + Duration::from_secs(60);
        reg.register(
            wid("w-a"),
            WorkerCapabilities {
                hostname: "h".to_string(),
                labels: BTreeMap::new(),
            },
            t0,
        );
        let views = worker_views(&reg, t0 - Duration::from_secs(30), "n", &|_| 0);
        assert_eq!(views[0].last_seen_secs, 0);
    }

    #[test]
    fn a_worker_past_the_heartbeat_deadline_is_stale() {
        let mut reg = WorkerRegistry::default();
        let t0 = Instant::now();
        reg.register(
            wid("w-a"),
            WorkerCapabilities {
                hostname: "h".to_string(),
                labels: BTreeMap::new(),
            },
            t0,
        );
        let deadline = reg.policy().deadline();
        let views = worker_views(&reg, t0 + deadline + Duration::from_secs(1), "n", &|_| 0);
        assert!(views[0].stale);
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

```bash
cargo test -p brokkr-control --lib views::worker
```

Expected: FAIL to compile — `views` module does not exist, `worker_views` not found.

- [ ] **Step 3: Write the module declaration**

Create `crates/brokkr-control/src/views/mod.rs`:

```rust
//! Read-model DTOs and pure projections for the observability surface
//! (ADR 0012).
//!
//! Every type here is a *view*: a snapshot of state shaped for an external
//! consumer, deliberately decoupled from the internal types it is derived
//! from. Internal state must not leak across this boundary — that rule is
//! what lets the scheduler and registry change shape without breaking the
//! wire format or the TUI.
//!
//! # `owning_node` is on every node-local DTO, on purpose
//!
//! In an HA cluster the worker registry, the job-history ring, the CAS and the
//! policy engine are all **per node** — see
//! `docs/operations/running-a-cluster.md`. Aggregation unions them, so each
//! record must say which node it came from or the merged view would present
//! three different local truths as one cluster fact.

mod policy;
mod worker;

pub use policy::{policy_view, PolicyView};
pub use worker::{worker_views, WorkerView};
```

Add to `crates/brokkr-control/src/lib.rs`, keeping the list alphabetical (after `pub mod services;`):

```rust
pub mod views;
```

- [ ] **Step 4: Implement the worker projection**

Prepend to `crates/brokkr-control/src/views/worker.rs`, above the test module:

```rust
//! Worker projections.

use std::collections::BTreeMap;
use std::time::Instant;

use brokkr_common::WorkerId;

use crate::registry::WorkerRegistry;

/// One worker, as an operator sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerView {
    /// The worker's id.
    pub worker_id: String,
    /// Hostname the worker declared at registration. May be empty.
    pub hostname: String,
    /// The worker's capability labels. `BTreeMap` so ordering is deterministic.
    pub labels: BTreeMap<String, String>,
    /// Jobs dispatched to this worker but not yet reported back.
    pub inflight: u32,
    /// Seconds since this worker was last heard from.
    ///
    /// A *relative* value, computed by the node that owns the record.
    /// `Instant` is monotonic and process-local: it has no defined epoch and
    /// two nodes' values are not comparable, so an absolute timestamp here
    /// would be meaningless once aggregated.
    pub last_seen_secs: u64,
    /// Whether the worker is past its heartbeat deadline.
    pub stale: bool,
    /// The control-plane node whose registry holds this worker.
    pub owning_node: String,
}

/// Project every worker in `registry` into a [`WorkerView`].
///
/// `inflight` is supplied as a closure rather than a handle so this stays a
/// pure function over borrowed state — the scheduler's in-flight counts live
/// behind a different lock, and taking that lock here would invert the
/// scheduler's lock order.
///
/// Output is sorted by worker id. `WorkerRegistry` iterates a `HashMap`, and
/// an unsorted read-model would present a different order on every call —
/// which this project has shipped twice already (#174, and the Phase 6
/// candidate ordering).
pub fn worker_views(
    registry: &WorkerRegistry,
    now: Instant,
    owning_node: &str,
    inflight: &dyn Fn(&WorkerId) -> usize,
) -> Vec<WorkerView> {
    let policy = registry.policy().clone();
    let mut views: Vec<WorkerView> = registry
        .iter()
        .map(|(id, record)| WorkerView {
            worker_id: id.as_str().to_string(),
            hostname: record.capabilities.hostname.clone(),
            labels: record.capabilities.labels.clone(),
            inflight: u32::try_from(inflight(id)).unwrap_or(u32::MAX),
            // `saturating_duration_since` so a `now` earlier than `last_seen`
            // reads as zero rather than panicking — the same posture
            // `WorkerRecord::is_stale` takes.
            last_seen_secs: now.saturating_duration_since(record.last_seen).as_secs(),
            stale: record.is_stale(now, &policy),
            owning_node: owning_node.to_string(),
        })
        .collect();
    views.sort_by(|a, b| a.worker_id.cmp(&b.worker_id));
    views
}
```

If `HeartbeatPolicy` does not derive `Clone`, add `#[derive(Clone)]` to it in
`crates/brokkr-control/src/registry.rs` rather than restructuring this function.

- [ ] **Step 5: Run the worker tests to verify they pass**

```bash
cargo test -p brokkr-control --lib views::worker
```

Expected: PASS, 5 tests.

- [ ] **Step 6: Write the failing test for the policy projection**

Create `crates/brokkr-control/src/views/policy.rs` with only the test module:

```rust
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

    /// Every reason tag is present even at zero, so a dashboard does not have
    /// a series appear out of nowhere the first time something breaks.
    #[test]
    fn all_failure_reasons_are_present_even_at_zero() {
        let v = policy_view(None, "n");
        // `policy_view(None, ..)` reports no reasons at all; a loaded policy
        // reports every reason. Both are covered — see the integration test in
        // Task 4, which exercises a real `WasmStrategy`.
        assert!(v.failures_by_reason.is_empty());

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
}
```

- [ ] **Step 7: Run it to verify it fails**

```bash
cargo test -p brokkr-control --lib views::policy
```

Expected: FAIL to compile — `policy_view` and `REASONS` not found.

- [ ] **Step 8: Implement the policy projection**

Prepend to `crates/brokkr-control/src/views/policy.rs`:

```rust
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
```

- [ ] **Step 9: Run the policy tests**

```bash
cargo test -p brokkr-control --lib views::policy
```

Expected: PASS, 2 tests.

- [ ] **Step 10: Run the full verification gate**

```bash
touch crates/*/src/lib.rs crates/*/src/main.rs
cargo fmt --all --check; echo "fmt=$?"
cargo clippy --all-targets --all-features -- -D warnings; echo "clippy=$?"
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features; echo "doc=$?"
cargo test --workspace --all-features --no-fail-fast > /tmp/t.log 2>&1; echo "test=$?"
grep -A10 "^failures:$" /tmp/t.log | grep -E "^    [a-z]" | sort -u
```

Expected: fmt/clippy/doc all `0`; the failure list is exactly the four
`evil_seccomp_caps` names from Global Constraints and nothing else.

- [ ] **Step 11: Update the CHANGELOG**

Add under `## [Unreleased]` → `### Added`:

```markdown
- **`brokkr-control::views`** — the read-model behind ADR 0012's observability
  surface. Pure projections from node-local state into DTOs, with `owning_node`
  on every one: in an HA cluster the worker registry, job history, CAS and
  policy engine are all per node, so aggregation must never present three
  local truths as one cluster fact. Worker liveness crosses as *seconds since
  last seen* rather than an `Instant`, which is monotonic, process-local, and
  not comparable between nodes.
```

- [ ] **Step 12: Commit**

```bash
git add crates/brokkr-control/src/views crates/brokkr-control/src/lib.rs CHANGELOG.md
git commit -m "feat(control): add the views read-model for worker and policy state

Motivation: ADR 0012's observability surface needs DTOs decoupled from
internal types. This is the first slice: workers and scheduling policy.

Two details that are load-bearing rather than incidental:

- Worker liveness crosses as seconds-since-last-seen, not as an Instant.
  Instant is monotonic and process-local with no defined epoch, so two nodes'
  values are not comparable at all — an absolute timestamp would be
  meaningless the moment it was aggregated.
- Every DTO carries owning_node. The registry, job history, CAS and policy
  engine are all per-node in an HA cluster, so a merged view that dropped the
  owner would present three different local truths as one cluster fact.

worker_views output is sorted by id. WorkerRegistry iterates a HashMap, and
this project has shipped unsorted-iteration bugs twice (#174 and the Phase 6
candidate ordering); a read-model that reordered on every call would be the
third.

How tested: 7 unit tests over pure functions — seconds-ago conversion,
clock skew reading as zero rather than panicking, staleness at the heartbeat
deadline, owning_node on every record, deterministic sort order, the
no-policy-configured case, and that every failure reason tag is enumerated.

Related: docs/superpowers/specs/2026-08-02-observability-read-model-design.md,
ADR 0012, ADR 0014."
```

---

### Task 2: `Cas::stats()` on a slow cadence

Branch: `feat/cas-stats` from `origin/main`.

**Files:**
- Modify: `crates/brokkr-cas/src/traits.rs`
- Modify: `crates/brokkr-cas/src/redb_backend.rs`
- Create: `crates/brokkr-control/src/views/cas.rs`
- Modify: `crates/brokkr-control/src/views/mod.rs`
- Modify: `CHANGELOG.md`

**Interfaces:**
- Consumes: `brokkr_cas::{Cas, CasError}`.
- Produces, relied on by Tasks 4 and 6:
  - `brokkr_cas::CasStats { objects: u64, bytes: u64 }`
  - `Cas::stats(&self) -> Result<CasStats, CasError>` (trait method, default impl)
  - `views::CasStatsView { objects: u64, bytes: u64, owning_node: String }`
  - `views::cas_stats_view(stats: CasStats, owning_node: &str) -> CasStatsView`

**Why this is its own task:** `RedbCas::list_digests` (`redb_backend.rs:168`)
scans the whole table inside `spawn_blocking` **while holding a permit from a
semaphore that returns `CasError::ThroughputLimit` when contended**. Deriving
stats from it on the 2s peer poll would be O(n) per poll and could steal a
permit from real CAS traffic. The stats call gets its own slower cadence in
Task 6; this task gives it a method that does not scan when it does not have to.

- [ ] **Step 1: Write the failing test for the default impl**

Add to the test module in `crates/brokkr-cas/src/traits.rs` (create
`#[cfg(test)] mod tests` at the end of the file if absent — note
`clippy::items_after_test_module` means **nothing may be appended after it**):

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::disallowed_methods, clippy::panic)]
mod tests {
    use bytes::Bytes;

    use super::*;
    use crate::in_memory::InMemoryCas;

    #[tokio::test]
    async fn stats_counts_objects_and_bytes() {
        let cas = InMemoryCas::new();
        let a = Bytes::from_static(b"hello");
        let b = Bytes::from_static(b"world!!");
        cas.batch_update_blobs(vec![
            (Digest::of(&a), a.clone()),
            (Digest::of(&b), b.clone()),
        ])
        .await
        .unwrap();

        let stats = cas.stats().await.unwrap();
        assert_eq!(stats.objects, 2);
        assert_eq!(stats.bytes, (a.len() + b.len()) as u64);
    }

    #[tokio::test]
    async fn stats_on_an_empty_cas_is_zero_not_an_error() {
        let stats = InMemoryCas::new().stats().await.unwrap();
        assert_eq!(stats.objects, 0);
        assert_eq!(stats.bytes, 0);
    }
}
```

- [ ] **Step 2: Run it to verify it fails**

```bash
cargo test -p brokkr-cas --lib traits::tests
```

Expected: FAIL to compile — no method `stats` on `Cas`.

- [ ] **Step 3: Add `CasStats` and the trait method**

In `crates/brokkr-cas/src/traits.rs`, add above the `Cas` trait:

```rust
/// Size of a single CAS store.
///
/// **Per store, never summed across nodes.** Each control-plane node opens its
/// own CAS, so three nodes holding one blob is three copies of one blob, not
/// three blobs. Adding these together reports storage that does not exist.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CasStats {
    /// Number of distinct blobs stored.
    pub objects: u64,
    /// Total bytes stored, summing each blob once.
    pub bytes: u64,
}
```

And inside the `Cas` trait, alongside the other defaulted methods:

```rust
    /// Size of this store.
    ///
    /// The default implementation derives stats from
    /// [`list_digests`](Cas::list_digests), which is a full scan on most
    /// backends. **Backends that can answer cheaply should override this** —
    /// callers may poll it, and `RedbCas` in particular takes a throughput
    /// permit for a scan that a poller could otherwise steal from real
    /// traffic.
    async fn stats(&self) -> Result<CasStats, CasError> {
        let digests = self.list_digests().await?;
        let bytes = digests
            .iter()
            .map(|d| u64::try_from(d.size_bytes()).unwrap_or(0))
            .sum();
        Ok(CasStats {
            objects: digests.len() as u64,
            bytes,
        })
    }
```

Export `CasStats` from `crates/brokkr-cas/src/lib.rs` alongside the existing
`Cas` / `CasError` re-exports.

- [ ] **Step 4: Run the trait tests**

```bash
cargo test -p brokkr-cas --lib traits::tests
```

Expected: PASS, 2 tests.

If `InMemoryCas::list_digests` is not overridden, the default returns an empty
`Vec` and `stats_counts_objects_and_bytes` will fail with `0 != 2`. In that
case override `list_digests` on `InMemoryCas` to return its keys — that is a
genuine gap in the fake, not a reason to weaken the test.

- [ ] **Step 5: Write the failing test for the `RedbCas` override**

Add to the test module in `crates/brokkr-cas/src/redb_backend.rs`:

```rust
    #[tokio::test]
    async fn redb_stats_match_a_manual_count() {
        let dir = tempfile::tempdir().unwrap();
        let cas = RedbCas::open(dir.path().join("cas.redb")).unwrap();
        let blobs: Vec<(Digest, Bytes)> = (0..5u8)
            .map(|i| {
                let b = Bytes::from(vec![i; (i as usize + 1) * 10]);
                (Digest::of(&b), b)
            })
            .collect();
        let expected_bytes: u64 = blobs.iter().map(|(_, b)| b.len() as u64).sum();
        cas.batch_update_blobs(blobs).await.unwrap();

        let stats = cas.stats().await.unwrap();
        assert_eq!(stats.objects, 5);
        assert_eq!(stats.bytes, expected_bytes);
    }
```

- [ ] **Step 6: Run it to verify it fails or passes via the default**

```bash
cargo test -p brokkr-cas --lib redb_stats_match_a_manual_count
```

Expected: PASS via the default impl (a scan). This confirms correctness before
the override changes the mechanism.

- [ ] **Step 7: Override `stats()` on `RedbCas`**

In the `impl Cas for RedbCas` block in `crates/brokkr-cas/src/redb_backend.rs`:

```rust
    async fn stats(&self) -> Result<CasStats, CasError> {
        // Still a scan — redb has no O(1) byte total — but taking the
        // throughput permit here rather than through `list_digests` avoids
        // materializing every `Digest` into a `Vec` just to sum it, which for
        // a large store is the difference between a transient allocation and
        // a large one.
        let _permit = self
            .semaphore
            .try_acquire()
            .map_err(|_| CasError::ThroughputLimit {
                limit: self.max_concurrent,
            })?;
        let db = self.db.clone();
        let span = tracing::info_span!("redb::stats");
        tokio::task::spawn_blocking(move || {
            let _guard = span.enter();
            let txn = db.begin_read()?;
            let table = txn.open_table(BLOBS)?;
            let mut objects = 0u64;
            let mut bytes = 0u64;
            for entry in table.iter()? {
                let (_k, v) = entry?;
                objects += 1;
                bytes += v.value().len() as u64;
            }
            Ok(CasStats { objects, bytes })
        })
        .await
        .map_err(|e| CasError::Other(format!("stats join: {e}")))?
    }
```

Match the existing error-mapping and `spawn_blocking` idiom in this file
exactly — copy the shape of the neighbouring `list_digests` implementation
rather than the sketch above if they differ.

- [ ] **Step 8: Re-run the redb test to confirm the override is equivalent**

```bash
cargo test -p brokkr-cas --lib redb_stats
```

Expected: PASS. Same numbers as Step 6, now via the override.

- [ ] **Step 9: Add the `CasStatsView` projection**

Create `crates/brokkr-control/src/views/cas.rs`:

```rust
//! CAS projections.

use brokkr_cas::CasStats;

/// One node's CAS size, as an operator sees it.
///
/// **Never summed across nodes.** Each control-plane node opens its own CAS
/// (`RedbCas::open(data_dir/cas.redb)`), so the same blob present on three
/// nodes is three copies of one blob. Adding the numbers together would report
/// storage that does not exist and a dedup ratio that means nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CasStatsView {
    /// Distinct blobs in this node's store.
    pub objects: u64,
    /// Bytes in this node's store.
    pub bytes: u64,
    /// The control-plane node this store belongs to.
    pub owning_node: String,
}

/// Project one node's [`CasStats`] into a [`CasStatsView`].
pub fn cas_stats_view(stats: CasStats, owning_node: &str) -> CasStatsView {
    CasStatsView {
        objects: stats.objects,
        bytes: stats.bytes,
        owning_node: owning_node.to_string(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::disallowed_methods, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn the_view_carries_the_owning_node() {
        let v = cas_stats_view(
            CasStats {
                objects: 3,
                bytes: 900,
            },
            "node-2",
        );
        assert_eq!(v.objects, 3);
        assert_eq!(v.bytes, 900);
        assert_eq!(v.owning_node, "node-2");
    }
}
```

Add to `crates/brokkr-control/src/views/mod.rs`:

```rust
mod cas;
pub use cas::{cas_stats_view, CasStatsView};
```

- [ ] **Step 10: Run the full verification gate**

Run the six commands from Global Constraints. Expected: fmt/clippy/doc `0`;
exactly the four `evil_seccomp_caps` failures.

- [ ] **Step 11: Update the CHANGELOG and commit**

Add under `## [Unreleased]` → `### Added`:

```markdown
- **`Cas::stats()`** returning `CasStats { objects, bytes }`, with a scanning
  default and a `RedbCas` override. Needed by ADR 0012's observability surface.
  Deliberately *not* derived from `list_digests()` on the polling path: that
  materializes every digest into a `Vec` and takes a throughput permit a poller
  could otherwise steal from real CAS traffic.
```

```bash
git add crates/brokkr-cas/src crates/brokkr-control/src/views CHANGELOG.md
git commit -m "feat(cas): add Cas::stats() for the observability read-model

Motivation: ADR 0012 assumes a CAS stats() method. There isn't one — only
list_digests(), which on RedbCas scans the whole table inside spawn_blocking
while holding a permit from a semaphore that returns ThroughputLimit when
contended. Deriving stats from it on a 2s poll would be O(n) per poll and
could steal a permit from real traffic.

What changed:
- CasStats { objects, bytes } and Cas::stats(), with a default derived from
  list_digests for backends that cannot do better.
- RedbCas overrides it to sum during the scan rather than materializing every
  Digest into a Vec first.
- views::CasStatsView, carrying owning_node like every other node-local DTO.

CasStats is documented as never-summable across nodes: each control-plane node
opens its own CAS, so one blob on three nodes is three copies, and adding the
numbers would report storage that does not exist.

How tested: the redb override is asserted equivalent to the scanning default
against the same fixture, so the optimization cannot silently change the
answer. Plus empty-store and owning-node cases.

Related: docs/superpowers/specs/2026-08-02-observability-read-model-design.md,
ADR 0012."
```

---

### Task 3: Raft state projection

Branch: `feat/views-raft` from `origin/main`.

**Files:**
- Create: `crates/brokkr-control/src/views/raft.rs`
- Modify: `crates/brokkr-control/src/views/mod.rs`
- Modify: `CHANGELOG.md`

**Interfaces:**
- Consumes: `brokkr_raft::DriverStatus` (fields `is_leader: bool`, `term: Term`, `commit_index: LogIndex`, `last_applied: LogIndex`, `last_log_index: LogIndex`, `leader: Option<NodeId>`, `config: ClusterConfig`).
- Produces, relied on by Tasks 4–6:
  - `views::RaftRole` — `Leader | Follower | Unknown`
  - `views::NodeView { node_id: String, advertise_addr: String, role: RaftRole, term: u64, commit_index: u64, last_applied: u64, reachable: bool, last_seen_secs: u64 }`
  - `views::node_view_from_status(node_id: &str, advertise_addr: &str, status: &DriverStatus) -> NodeView`
  - `views::unreachable_node_view(node_id: &str, advertise_addr: &str) -> NodeView`

- [ ] **Step 1: Write the failing tests**

Create `crates/brokkr-control/src/views/raft.rs` with only the test module:

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::disallowed_methods, clippy::panic)]
mod tests {
    use super::*;

    fn status(is_leader: bool, leader: Option<&str>) -> DriverStatus {
        DriverStatus {
            is_leader,
            term: 7,
            commit_index: 42,
            last_applied: 41,
            last_log_index: 43,
            leader: leader.map(|s| s.to_string()),
            snapshot: None,
            config: Default::default(),
        }
    }

    #[test]
    fn a_leader_projects_as_leader() {
        let v = node_view_from_status("node-1", "10.0.0.1:7878", &status(true, Some("node-1")));
        assert_eq!(v.node_id, "node-1");
        assert_eq!(v.advertise_addr, "10.0.0.1:7878");
        assert_eq!(v.role, RaftRole::Leader);
        assert_eq!(v.term, 7);
        assert_eq!(v.commit_index, 42);
        assert_eq!(v.last_applied, 41);
        assert!(v.reachable);
    }

    #[test]
    fn a_node_that_recognises_a_leader_projects_as_follower() {
        let v = node_view_from_status("node-2", "10.0.0.2:7878", &status(false, Some("node-1")));
        assert_eq!(v.role, RaftRole::Follower);
    }

    /// A node in an election, or partitioned from the leader, recognises
    /// nobody. That is distinct from being a follower and must not be
    /// flattened into it — "nobody is leading" is exactly what an operator
    /// needs to see during an incident.
    #[test]
    fn a_node_recognising_no_leader_is_unknown_not_follower() {
        let v = node_view_from_status("node-3", "10.0.0.3:7878", &status(false, None));
        assert_eq!(v.role, RaftRole::Unknown);
    }

    /// An unreachable node still appears, with its identity and zeroed state.
    /// Dropping it would make "a node I know about is not answering"
    /// indistinguishable from "that node does not exist".
    #[test]
    fn an_unreachable_node_is_present_but_marked() {
        let v = unreachable_node_view("node-4", "10.0.0.4:7878");
        assert_eq!(v.node_id, "node-4");
        assert_eq!(v.advertise_addr, "10.0.0.4:7878");
        assert!(!v.reachable);
        assert_eq!(v.role, RaftRole::Unknown);
        assert_eq!(v.term, 0);
        assert_eq!(v.commit_index, 0);
    }
}
```

- [ ] **Step 2: Run it to verify it fails**

```bash
cargo test -p brokkr-control --lib views::raft
```

Expected: FAIL to compile — `RaftRole`, `node_view_from_status`, `unreachable_node_view` not found.

- [ ] **Step 3: Implement the projection**

Prepend to `crates/brokkr-control/src/views/raft.rs`:

```rust
//! Raft state projections (ADR 0013).

use brokkr_raft::DriverStatus;

/// A node's role in the Raft cluster, as an operator sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RaftRole {
    /// This node believes it is the leader.
    Leader,
    /// This node recognises some other node as leader.
    Follower,
    /// This node recognises no leader — mid-election, or partitioned from one.
    ///
    /// Deliberately distinct from [`Self::Follower`]. "Nobody is leading" is
    /// exactly what an operator needs to see during an incident, and folding
    /// it into `Follower` would hide it.
    Unknown,
}

/// One control-plane node, as an operator sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeView {
    /// The node's Raft id.
    pub node_id: String,
    /// The address this node advertises to peers and clients.
    pub advertise_addr: String,
    /// The node's Raft role.
    pub role: RaftRole,
    /// The node's current Raft term.
    pub term: u64,
    /// The highest log index the node knows to be committed.
    pub commit_index: u64,
    /// The highest index applied to the state machine. Lag behind
    /// `commit_index` is normal and transient; sustained lag is not.
    pub last_applied: u64,
    /// Whether this node answered the most recent poll.
    pub reachable: bool,
    /// Seconds since this node last answered. Zero when it just did.
    pub last_seen_secs: u64,
}

/// Project a live node's [`DriverStatus`].
pub fn node_view_from_status(
    node_id: &str,
    advertise_addr: &str,
    status: &DriverStatus,
) -> NodeView {
    let role = if status.is_leader {
        RaftRole::Leader
    } else if status.leader.is_some() {
        RaftRole::Follower
    } else {
        RaftRole::Unknown
    };
    NodeView {
        node_id: node_id.to_string(),
        advertise_addr: advertise_addr.to_string(),
        role,
        term: status.term,
        commit_index: status.commit_index,
        last_applied: status.last_applied,
        reachable: true,
        last_seen_secs: 0,
    }
}

/// A node that is known to the cluster configuration but did not answer.
///
/// Present rather than omitted: dropping it would make "a node I know about is
/// not answering" indistinguishable from "that node does not exist", which is
/// the difference between a degraded cluster and a smaller one.
pub fn unreachable_node_view(node_id: &str, advertise_addr: &str) -> NodeView {
    NodeView {
        node_id: node_id.to_string(),
        advertise_addr: advertise_addr.to_string(),
        role: RaftRole::Unknown,
        term: 0,
        commit_index: 0,
        last_applied: 0,
        reachable: false,
        last_seen_secs: 0,
    }
}
```

If `Term`, `LogIndex` or `NodeId` are newtypes rather than bare `u64`/`String`,
convert at this boundary with their accessors — the DTO must expose plain
scalars so the proto mapping in Task 4 is mechanical. Check
`crates/brokkr-raft/src/types.rs` and adjust the field reads accordingly; do
not change the DTO's field types.

- [ ] **Step 4: Run the tests**

```bash
cargo test -p brokkr-control --lib views::raft
```

Expected: PASS, 4 tests.

- [ ] **Step 5: Wire the module**

Add to `crates/brokkr-control/src/views/mod.rs`:

```rust
mod raft;
pub use raft::{node_view_from_status, unreachable_node_view, NodeView, RaftRole};
```

Add `brokkr-raft.workspace = true` to `crates/brokkr-control/Cargo.toml` only if
it is not already a dependency — it is, so no manifest change should be needed.

- [ ] **Step 6: Run the full verification gate**

Run the six commands from Global Constraints. Expected: fmt/clippy/doc `0`;
exactly the four `evil_seccomp_caps` failures.

- [ ] **Step 7: Update the CHANGELOG and commit**

Add under `## [Unreleased]` → `### Added`:

```markdown
- **Raft state in the `views` read-model** — `NodeView` and `RaftRole`,
  projecting `DriverStatus` into role, term, commit index and applied index.
  `RaftRole::Unknown` is deliberately distinct from `Follower`: a node
  recognising no leader is mid-election or partitioned, which is precisely what
  an operator needs to see during an incident.
```

```bash
git add crates/brokkr-control/src/views CHANGELOG.md
git commit -m "feat(control): project Raft state into the views read-model

Motivation: ADR 0012 predates Phase 5 and its DTO list has no Raft state. In an
HA cluster, who is leader at what term with what commit index is arguably the
most valuable thing an operator can see, and DriverStatus already carries all
of it — this is a projection, not new bookkeeping.

Two decisions worth stating:

- RaftRole::Unknown is distinct from Follower. A node recognising no leader is
  mid-election or partitioned from one; folding that into Follower would hide
  the single most important signal during an incident.
- unreachable_node_view keeps an unreachable node present with zeroed state
  rather than omitting it, so 'a node I know about is not answering' stays
  distinguishable from 'that node does not exist'. That is the difference
  between a degraded cluster and a smaller one.

How tested: 4 unit tests over the pure projection — leader, follower, the
no-leader-recognised case, and the unreachable placeholder.

Related: docs/superpowers/specs/2026-08-02-observability-read-model-design.md,
ADR 0012, ADR 0013."
```

---

### Task 4: `observability.proto` and a local-only `ObservabilityService`

Branch: `feat/observability-service` from `origin/main`.

**Files:**
- Create: `crates/brokkr-proto/protos/brokkr/v1/observability.proto`
- Modify: `crates/brokkr-proto/build.rs` (add to the `protos` array)
- Create: `crates/brokkr-control/src/services/observability.rs`
- Modify: `crates/brokkr-control/src/services/mod.rs`
- Modify: `crates/brokkr-control/src/main.rs`
- Create: `crates/brokkr-control/tests/observability_listener.rs`
- Modify: `CHANGELOG.md`

**Interfaces:**
- Consumes: everything from Tasks 1–3.
- Produces, relied on by Tasks 5–8:
  - proto messages `NodeInfo`, `WorkerInfo`, `PolicyInfo`, `CasInfo`, `ClusterInfo`, and the requests/replies below
  - `services::ObservabilityService::new(deps: ObservabilityDeps) -> Self`
  - ```rust
    /// Handles the service reads from. `raft` is `None` when `--raft` is off,
    /// in which case the node reports itself as a single unknown-role member.
    pub struct ObservabilityDeps {
        pub node_id: String,
        pub advertise_addr: String,
        pub registry: SharedWorkerRegistry,
        pub scheduler: Arc<Scheduler>,
        pub cas: Arc<dyn Cas>,
        pub policy: Option<Arc<WasmStrategy>>,
        /// `brokkr_raft::RaftHandle`, whose `status()` yields `DriverStatus`.
        pub raft: Option<Arc<brokkr_raft::RaftHandle>>,
    }
    ```

**This task serves local state only.** Aggregation arrives in Task 6. That
ordering is deliberate: it means the service, its listener, and its auth posture
are all provable before any distributed behaviour exists to confuse a failure.

- [ ] **Step 1: Write the proto**

Create `crates/brokkr-proto/protos/brokkr/v1/observability.proto`:

```proto
// Brokkr operator observability (ADR 0012).
//
// Read-only. Served on a dedicated operator listener, never on the
// tenant-facing port — ADR 0011's auth has no scope concept, so a tenant token
// reaching this service could enumerate every worker and every other tenant's
// jobs.
syntax = "proto3";

package brokkr.v1;

// One control-plane node.
message NodeInfo {
  string node_id = 1;
  string advertise_addr = 2;
  // "leader" | "follower" | "unknown". A string rather than an enum so adding
  // a role later cannot renumber an existing one.
  string role = 3;
  uint64 term = 4;
  uint64 commit_index = 5;
  uint64 last_applied = 6;
  // False when this node did not answer the most recent poll. It still
  // appears, so "known but silent" stays distinct from "not a member".
  bool reachable = 7;
  uint64 last_seen_secs = 8;
}

// One worker. Always carries the node whose registry holds it: the registry is
// per-node, so this is never a cluster-wide fact.
message WorkerInfo {
  string worker_id = 1;
  string hostname = 2;
  map<string, string> labels = 3;
  uint32 inflight = 4;
  // Seconds since last heard from, computed by the owning node. Relative
  // because Instant is monotonic and process-local — an absolute timestamp
  // would not be comparable between nodes.
  uint64 last_seen_secs = 5;
  bool stale = 6;
  string owning_node = 7;
}

// One node's scheduling-policy state (ADR 0014). Per node: nodes may
// legitimately have different policies loaded, or differ in quarantine state.
message PolicyInfo {
  bool loaded = 1;
  bool quarantined = 2;
  uint64 decided = 3;
  uint64 declined = 4;
  map<string, uint64> failures_by_reason = 5;
  string owning_node = 6;
}

// One node's CAS size. NEVER summed across nodes: each node opens its own
// store, so one blob on three nodes is three copies of one blob.
message CasInfo {
  uint64 objects = 1;
  uint64 bytes = 2;
  string owning_node = 3;
}

message ClusterInfo {
  repeated NodeInfo nodes = 1;
  string leader_id = 2;
  bool quorum_healthy = 3;
  // True when any known node did not answer the most recent poll.
  bool degraded = 4;
  // Unix seconds when the snapshot was taken. Zero before the first poll.
  uint64 as_of_unix_secs = 5;
}

message GetClusterRequest {}
message GetClusterReply { ClusterInfo cluster = 1; }

message ListWorkersRequest {}
message ListWorkersReply { repeated WorkerInfo workers = 1; }

message GetPolicyRequest {}
message GetPolicyReply { repeated PolicyInfo policies = 1; }

message GetCasStatsRequest {}
message GetCasStatsReply { repeated CasInfo stores = 1; }

service ObservabilityService {
  rpc GetCluster(GetClusterRequest) returns (GetClusterReply);
  rpc ListWorkers(ListWorkersRequest) returns (ListWorkersReply);
  rpc GetPolicy(GetPolicyRequest) returns (GetPolicyReply);
  rpc GetCasStats(GetCasStatsRequest) returns (GetCasStatsReply);
}
```

`GetPolicyReply` and `GetCasStatsReply` return **repeated** values, one per
node, precisely because these must never be combined into a single number.
`ListJobs` / `GetJob` / `GetWorker` / `WatchEvents` arrive in later tasks; this
task establishes the surface.

- [ ] **Step 2: Register the proto and verify codegen**

In `crates/brokkr-proto/build.rs`, add to the `protos` array after
`"brokkr/v1/policy.proto"`:

```rust
        "brokkr/v1/observability.proto",
```

```bash
cargo build -p brokkr-proto
```

Expected: builds clean. The generated types land in `brokkr_proto::brokkr_v1`.

- [ ] **Step 3: Write the failing test that the service is not on the tenant port**

Create `crates/brokkr-control/tests/observability_listener.rs`:

```rust
//! The operator listener is separate from the tenant-facing listener.
//!
//! ADR 0011's auth has no scope concept — `Authenticator::authenticate`
//! returns a `TenantId` and nothing else. If `ObservabilityService` were
//! mounted on the client port, any tenant's token could enumerate every worker
//! and every other tenant's jobs. The entire security argument for D4 in
//! `docs/superpowers/specs/2026-08-02-observability-read-model-design.md`
//! rests on this separation, so it gets its own test.

#![allow(clippy::unwrap_used, clippy::disallowed_methods, clippy::panic)]

use brokkr_proto::brokkr_v1::observability_service_client::ObservabilityServiceClient;
use brokkr_proto::brokkr_v1::GetClusterRequest;

mod harness;

#[tokio::test]
async fn observability_answers_on_the_operator_listener() {
    let cluster = harness::start_single_node().await;
    let mut client = ObservabilityServiceClient::connect(cluster.observe_endpoint())
        .await
        .unwrap();
    let reply = client
        .get_cluster(GetClusterRequest {})
        .await
        .unwrap()
        .into_inner();
    let info = reply.cluster.unwrap();
    assert_eq!(info.nodes.len(), 1, "a single node reports exactly itself");
    assert!(!info.degraded);
}

#[tokio::test]
async fn observability_is_not_reachable_on_the_tenant_listener() {
    let cluster = harness::start_single_node().await;
    // Connecting may succeed — it is the same transport — but the service must
    // not be routed there.
    let connected = ObservabilityServiceClient::connect(cluster.client_endpoint()).await;
    let Ok(mut client) = connected else {
        // Refusing the connection outright is also an acceptable outcome.
        return;
    };
    let status = client
        .get_cluster(GetClusterRequest {})
        .await
        .expect_err("ObservabilityService must not be served on the tenant port");
    assert_eq!(
        status.code(),
        tonic::Code::Unimplemented,
        "expected the tenant listener to not implement this service; got {status:?}"
    );
}
```

Create `crates/brokkr-control/tests/harness/mod.rs` providing
`start_single_node()` returning a handle with `client_endpoint()` and
`observe_endpoint()`. Model it on the existing in-process boot in
`crates/brokkr-control/tests/leader_redirect.rs` — read that file first and
reuse its server-spawn shape rather than inventing a second one.

- [ ] **Step 4: Run it to verify it fails**

```bash
cargo test -p brokkr-control --test observability_listener
```

Expected: FAIL to compile — no `ObservabilityService`, no `observe_endpoint`.

- [ ] **Step 5: Implement the service over local state**

Create `crates/brokkr-control/src/services/observability.rs`. It maps `views`
DTOs to proto messages and nothing else — no state of its own, no aggregation
yet. Register it in `crates/brokkr-control/src/services/mod.rs` next to the
existing service re-exports.

The mapping is mechanical; `RaftRole` becomes the string `"leader"`,
`"follower"` or `"unknown"` to match the proto comment.

- [ ] **Step 6: Add the operator listener to `main.rs`**

Add the flag alongside the Phase 6 policy flags:

```rust
    /// Bind address for the operator observability listener (ADR 0012).
    ///
    /// Deliberately **not** the tenant-facing `--listen` port. ADR 0011's auth
    /// resolves a token to a tenant and has no scope concept, so a tenant
    /// reaching this service could enumerate every worker and every other
    /// tenant's jobs. Defaults to loopback; expose it deliberately or not at
    /// all.
    #[arg(long, default_value = "127.0.0.1:7880")]
    observe_listen: SocketAddr,
```

Spawn a second `Server::builder()` bound to `args.observe_listen` serving only
`ObservabilityServiceServer`. Do **not** add the client auth interceptor to it:
the listener *is* the boundary, and adding a tenant-resolving interceptor to an
operator surface would imply a tenant scope that does not exist.

Log the binding at startup with the same prominence as the TLS posture
warnings:

```rust
    tracing::info!(
        addr = %args.observe_listen,
        "operator observability listener bound (read-only, no tenant auth — \
         do not expose this port to tenants)"
    );
```

- [ ] **Step 7: Run the listener tests**

```bash
cargo test -p brokkr-control --test observability_listener
```

Expected: PASS, 2 tests.

- [ ] **Step 8: Run the full verification gate**

Run the six commands from Global Constraints.

- [ ] **Step 9: Update the CHANGELOG and commit**

Add under `## [Unreleased]` → `### Added`:

```markdown
- **`brokkr.v1.ObservabilityService`** on a dedicated operator listener
  (`--observe-listen`, default `127.0.0.1:7880`) — read-only `GetCluster`,
  `ListWorkers`, `GetPolicy`, `GetCasStats`. Deliberately not on the
  tenant-facing port: ADR 0011's auth resolves a token to a tenant and has no
  scope concept, so a tenant reaching this service could enumerate every worker
  and every other tenant's jobs. `GetPolicy` and `GetCasStats` return one entry
  *per node* rather than a combined figure, because neither is summable.
```

```bash
git add crates/brokkr-proto crates/brokkr-control CHANGELOG.md
git commit -m "feat(control): serve ObservabilityService on an operator listener

Motivation: ADR 0012 needs a read API. It specifies mounting it 'behind the
same auth interceptor as a read-only scope' — but ADR 0011 has no scope
concept: Authenticator::authenticate returns a TenantId and nothing else.
Behind the existing interceptor unchanged, any tenant's token could enumerate
every worker and every other tenant's job metadata, a regression against
ADR 0010's multi-tenancy posture.

So the listener is the boundary: a separate bind address, defaulting to
loopback, serving only this service and carrying no tenant-resolving
interceptor. Adding one would imply a scope that does not exist.

This increment serves node-local state only; aggregation lands separately. That
ordering is deliberate — the service, its listener and its auth posture are all
provable before any distributed behaviour exists to confuse a failure.

GetPolicy and GetCasStats return repeated per-node entries rather than a single
combined figure, because neither is summable: each node opens its own CAS and
runs its own policy engine.

How tested: an integration test asserts the service answers on the operator
endpoint and is NOT served on the tenant endpoint. D4's whole security argument
rests on that separation, so it is pinned rather than assumed.

Related: docs/superpowers/specs/2026-08-02-observability-read-model-design.md,
ADR 0012, ADR 0011, ADR 0010."
```

---

### Task 5: `PeerObservability` on the Raft plane

Branch: `feat/peer-observability` from `origin/main`.

**Files:**
- Modify: `crates/brokkr-proto/protos/brokkr/v1/raft.proto`
- Create: `crates/brokkr-control/src/services/peer_observability.rs`
- Modify: `crates/brokkr-control/src/services/mod.rs`
- Modify: `crates/brokkr-control/src/main.rs`
- Create: `crates/brokkr-control/tests/peer_observability.rs`
- Modify: `CHANGELOG.md`

**Interfaces:**
- Produces, relied on by Task 6:
  - `service PeerObservability { rpc GetLocalState(GetLocalStateRequest) returns (GetLocalStateReply); }`
  - `GetLocalStateReply { NodeInfo node = 1; repeated WorkerInfo workers = 2; PolicyInfo policy = 3; CasInfo cas = 4; }`

- [ ] **Step 1: Add the peer service to `raft.proto`**

Append to `crates/brokkr-proto/protos/brokkr/v1/raft.proto`:

```proto
// Node-local observability, for peer aggregation (ADR 0012).
//
// Lives on the Raft peer plane because peers there are already mutually
// authenticated by mTLS and their addresses are already published in the
// cluster configuration — no new credential, and nothing on the tenant-facing
// surface.
//
// This service returns THIS NODE'S state and nothing else. It contains no
// fan-out path, which is the structural guarantee that aggregation cannot
// recurse: a flag can be forgotten or spoofed, a service with no recursion
// path cannot be made to recurse.
message GetLocalStateRequest {}

message GetLocalStateReply {
  NodeInfo node = 1;
  repeated WorkerInfo workers = 2;
  PolicyInfo policy = 3;
  CasInfo cas = 4;
}

service PeerObservability {
  rpc GetLocalState(GetLocalStateRequest) returns (GetLocalStateReply);
}
```

`raft.proto` and `observability.proto` are both `package brokkr.v1`, so
`NodeInfo` and friends resolve without an import. If `prost` reports otherwise,
add `import "brokkr/v1/observability.proto";` at the top of `raft.proto`.

- [ ] **Step 2: Write the failing test that it cannot fan out**

Create `crates/brokkr-control/tests/peer_observability.rs`:

```rust
//! `PeerObservability` returns node-local state and never fans out.
//!
//! The no-recursion guarantee for aggregation is structural rather than a
//! flag: this service has no code path that calls a peer. A test pins it so a
//! future refactor cannot quietly add one.

#![allow(clippy::unwrap_used, clippy::disallowed_methods, clippy::panic)]

/// The implementation must not reference any peer client type. This is a
/// source-level assertion because the property is "no such code path exists",
/// which no runtime test can demonstrate.
#[test]
fn the_peer_service_has_no_client_dependency() {
    let src = include_str!("../src/services/peer_observability.rs");
    for forbidden in [
        "PeerObservabilityClient",
        "ObservabilityServiceClient",
        "ClusterSnapshot",
        "poller",
    ] {
        assert!(
            !src.contains(forbidden),
            "peer_observability.rs references `{forbidden}`; this service must \
             return node-local state only and must never fan out"
        );
    }
}
```

- [ ] **Step 3: Run it to verify it fails**

```bash
cargo test -p brokkr-control --test peer_observability
```

Expected: FAIL — `peer_observability.rs` does not exist, so `include_str!` fails to compile.

- [ ] **Step 4: Implement the peer service**

Create `crates/brokkr-control/src/services/peer_observability.rs`. It reuses the
same `views` projections as Task 4 and returns one node's state. Keep it
deliberately small — the file's brevity is part of the guarantee.

- [ ] **Step 5: Mount it on the Raft server in `main.rs`**

Find where `RaftServiceServer` is added to the Raft-plane `Server::builder()`
and add `PeerObservabilityServer` to the same builder, so it inherits the
peer-plane mTLS configuration (`RaftPlaneTls`). Do not add it to the client or
worker listeners.

- [ ] **Step 6: Run the test**

```bash
cargo test -p brokkr-control --test peer_observability
```

Expected: PASS.

- [ ] **Step 7: Run the full verification gate, update CHANGELOG, commit**

Add under `## [Unreleased]` → `### Added`:

```markdown
- **`brokkr.v1.PeerObservability`** on the Raft peer plane — one RPC,
  `GetLocalState`, returning this node's observability state for peer
  aggregation. It lives on the peer plane because peers there are already
  mutually authenticated by mTLS with addresses already published in the
  cluster configuration: no new credential, and nothing added to the
  tenant-facing surface. It contains no fan-out path, which is what makes the
  no-recursion guarantee structural rather than a flag a refactor could forget.
```

```bash
git add crates/brokkr-proto crates/brokkr-control CHANGELOG.md
git commit -m "feat(control): add PeerObservability on the Raft peer plane

Motivation: aggregation needs to ask peers for their local state. Doing that
over the client plane would mean either relaying the operator's token (making
the control plane a token relay a compromised node could replay) or issuing a
fourth credential for one read path. The Raft peer plane already has mutual
mTLS and already publishes peer addresses in the cluster configuration.

The service returns this node's state and contains no fan-out code path. That
is the no-recursion guarantee, and it is structural on purpose: a 'do not fan
out' flag can be forgotten, mis-defaulted, or spoofed, whereas a service with
no recursion path cannot be made to recurse.

How tested: a source-level assertion that the implementation references no peer
client type, no snapshot, and no poller. The property being asserted is 'no
such code path exists', which no runtime test can demonstrate — so the test
reads the source. It will fail loudly if a future refactor adds one.

Related: docs/superpowers/specs/2026-08-02-observability-read-model-design.md
D2 and D5, ADR 0012."
```

---

### Task 6: The cluster poller and aggregation rules

Branch: `feat/cluster-poller` from `origin/main`.

**Files:**
- Create: `crates/brokkr-control/src/cluster/mod.rs`
- Create: `crates/brokkr-control/src/cluster/aggregate.rs`
- Create: `crates/brokkr-control/src/cluster/probe.rs`
- Modify: `crates/brokkr-control/src/lib.rs` (add `pub mod cluster;` before `pub mod fairqueue;`)
- Modify: `crates/brokkr-control/src/services/observability.rs`
- Modify: `crates/brokkr-control/src/main.rs`
- Modify: `CHANGELOG.md`

**Interfaces:**
- Consumes: `views::{NodeView, WorkerView, PolicyView, CasStatsView, RaftRole, unreachable_node_view}` (Tasks 1–3); the generated `peer_observability_client::PeerObservabilityClient` (Task 5).
- Produces, relied on by Tasks 7–9:

```rust
/// One node's complete observability state — this node's own, or a peer's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeState {
    pub node: NodeView,
    pub workers: Vec<WorkerView>,
    pub policy: PolicyView,
    pub cas: CasStatsView,
}

/// The result of asking one peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerOutcome {
    /// The peer answered.
    Answered(NodeState),
    /// The peer is a known cluster member that did not answer.
    Unreachable { node_id: String, advertise_addr: String },
}

pub struct ClusterSnapshot {
    pub nodes: Vec<NodeView>,
    pub workers: Vec<WorkerView>,
    pub policies: Vec<PolicyView>,
    pub cas: Vec<CasStatsView>,
    pub leader_id: Option<String>,
    pub degraded: bool,
    pub as_of: Option<SystemTime>,
}

pub type SharedSnapshot = Arc<RwLock<ClusterSnapshot>>;

pub fn merge(local: NodeState, peers: Vec<PeerOutcome>, as_of: SystemTime) -> ClusterSnapshot;
pub fn spawn_poller(shared: SharedSnapshot, probe: Arc<dyn PeerProbe>, cfg: PollerConfig)
    -> tokio::task::JoinHandle<()>;
```

The aggregation rules are pure and are tested first. They are the part most
likely to be got wrong and the part least convenient to test through a socket.

- [ ] **Step 1: Write the failing aggregation tests**

Create `crates/brokkr-control/src/cluster/aggregate.rs` containing only this
test module:

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::disallowed_methods, clippy::panic)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::{Duration, SystemTime};

    use super::*;
    use crate::views::{CasStatsView, NodeView, PolicyView, RaftRole, WorkerView};

    fn at() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
    }

    fn node(id: &str, role: RaftRole) -> NodeView {
        NodeView {
            node_id: id.to_string(),
            advertise_addr: format!("10.0.0.1:{}", 7000 + id.len()),
            role,
            term: 7,
            commit_index: 42,
            last_applied: 42,
            reachable: true,
            last_seen_secs: 0,
        }
    }

    fn worker(id: &str, owner: &str) -> WorkerView {
        WorkerView {
            worker_id: id.to_string(),
            hostname: id.to_string(),
            labels: BTreeMap::new(),
            inflight: 0,
            last_seen_secs: 1,
            stale: false,
            owning_node: owner.to_string(),
        }
    }

    fn policy(owner: &str, quarantined: bool) -> PolicyView {
        PolicyView {
            loaded: true,
            quarantined,
            decided: 5,
            declined: 1,
            failures_by_reason: BTreeMap::new(),
            owning_node: owner.to_string(),
        }
    }

    fn cas(owner: &str) -> CasStatsView {
        CasStatsView {
            objects: 10,
            bytes: 1000,
            owning_node: owner.to_string(),
        }
    }

    fn state(id: &str, role: RaftRole, workers: &[&str]) -> NodeState {
        NodeState {
            node: node(id, role),
            workers: workers.iter().map(|w| worker(w, id)).collect(),
            policy: policy(id, false),
            cas: cas(id),
        }
    }

    /// The whole point of fan-out: workers from every node appear exactly
    /// once, each labelled with the node that knows about it.
    #[test]
    fn workers_from_all_nodes_are_unioned_and_labelled() {
        let snap = merge(
            state("node-1", RaftRole::Leader, &["w-a"]),
            vec![
                PeerOutcome::Answered(state("node-2", RaftRole::Follower, &["w-b"])),
                PeerOutcome::Answered(state("node-3", RaftRole::Follower, &["w-c"])),
            ],
            at(),
        );
        let ids: Vec<&str> = snap.workers.iter().map(|w| w.worker_id.as_str()).collect();
        assert_eq!(ids, vec!["w-a", "w-b", "w-c"]);
        assert_eq!(snap.workers[0].owning_node, "node-1");
        assert_eq!(snap.workers[1].owning_node, "node-2");
        assert_eq!(snap.workers[2].owning_node, "node-3");
        assert!(!snap.degraded);
    }

    /// CAS stats are NEVER summed. Each node opens its own store, so the same
    /// blob on three nodes is three copies of one blob — a total would report
    /// storage that does not exist and a dedup ratio that means nothing.
    #[test]
    fn cas_stats_are_reported_per_node_never_summed() {
        let snap = merge(
            state("node-1", RaftRole::Leader, &[]),
            vec![
                PeerOutcome::Answered(state("node-2", RaftRole::Follower, &[])),
                PeerOutcome::Answered(state("node-3", RaftRole::Follower, &[])),
            ],
            at(),
        );
        assert_eq!(snap.cas.len(), 3, "one entry per node, not one total");
        assert!(
            snap.cas.iter().all(|c| c.objects == 10 && c.bytes == 1000),
            "per-node values must be preserved verbatim, not combined"
        );
        let owners: Vec<&str> = snap.cas.iter().map(|c| c.owning_node.as_str()).collect();
        assert_eq!(owners, vec!["node-1", "node-2", "node-3"]);
    }

    /// Policy counters are per node for the same reason: nodes may have
    /// different modules loaded, or differ in quarantine state. Two nodes
    /// disagreeing is real information, not a glitch to average away.
    #[test]
    fn policy_views_are_reported_per_node_never_summed() {
        let mut quarantined = state("node-2", RaftRole::Follower, &[]);
        quarantined.policy = policy("node-2", true);
        let snap = merge(
            state("node-1", RaftRole::Leader, &[]),
            vec![PeerOutcome::Answered(quarantined)],
            at(),
        );
        assert_eq!(snap.policies.len(), 2);
        assert!(!snap.policies[0].quarantined);
        assert!(snap.policies[1].quarantined);
        assert_eq!(snap.policies[1].owning_node, "node-2");
    }

    /// Leadership comes from Raft — the node that says it is leading — not
    /// from counting agreement among replies.
    #[test]
    fn the_leader_is_taken_from_raft_not_from_a_majority_of_replies() {
        let snap = merge(
            state("node-1", RaftRole::Follower, &[]),
            vec![
                PeerOutcome::Answered(state("node-2", RaftRole::Leader, &[])),
                PeerOutcome::Answered(state("node-3", RaftRole::Follower, &[])),
            ],
            at(),
        );
        assert_eq!(snap.leader_id.as_deref(), Some("node-2"));
        assert!(!snap.degraded);
    }

    /// One unreachable peer degrades the snapshot without failing it, and the
    /// missing node still appears — "known but silent" must stay distinct from
    /// "not a member".
    #[test]
    fn an_unreachable_peer_degrades_but_does_not_remove_the_node() {
        let snap = merge(
            state("node-1", RaftRole::Leader, &["w-a"]),
            vec![
                PeerOutcome::Answered(state("node-2", RaftRole::Follower, &["w-b"])),
                PeerOutcome::Unreachable {
                    node_id: "node-3".to_string(),
                    advertise_addr: "10.0.0.3:7878".to_string(),
                },
            ],
            at(),
        );
        assert!(snap.degraded);
        assert_eq!(snap.nodes.len(), 3, "the silent node must still be listed");
        let n3 = snap.nodes.iter().find(|n| n.node_id == "node-3").unwrap();
        assert!(!n3.reachable);
        assert_eq!(n3.advertise_addr, "10.0.0.3:7878");
        // Its workers are simply absent — we cannot know them.
        assert_eq!(snap.workers.len(), 2);
        // Leadership is still known, because the leader answered.
        assert_eq!(snap.leader_id.as_deref(), Some("node-1"));
    }

    /// Two nodes both claiming leadership means we are mid-election or
    /// partitioned. Reporting one of them arbitrarily would be a confident
    /// lie; report none and mark degraded.
    #[test]
    fn two_claimed_leaders_report_no_leader_and_degraded() {
        let snap = merge(
            state("node-1", RaftRole::Leader, &[]),
            vec![PeerOutcome::Answered(state("node-2", RaftRole::Leader, &[]))],
            at(),
        );
        assert_eq!(snap.leader_id, None);
        assert!(snap.degraded);
    }

    /// No node claiming leadership is equally worth surfacing — that is an
    /// election in progress, and it is exactly what an operator wants to see.
    #[test]
    fn no_claimed_leader_reports_none_and_degraded() {
        let snap = merge(
            state("node-1", RaftRole::Unknown, &[]),
            vec![PeerOutcome::Answered(state("node-2", RaftRole::Unknown, &[]))],
            at(),
        );
        assert_eq!(snap.leader_id, None);
        assert!(snap.degraded);
    }

    /// Output ordering must not depend on which peer answered first. Replies
    /// arrive in completion order, which is nondeterministic; this project has
    /// shipped ordering bugs from exactly that class twice.
    #[test]
    fn merge_output_is_independent_of_reply_order() {
        let local = state("node-2", RaftRole::Leader, &["w-b"]);
        let p1 = PeerOutcome::Answered(state("node-1", RaftRole::Follower, &["w-a"]));
        let p3 = PeerOutcome::Answered(state("node-3", RaftRole::Follower, &["w-c"]));

        let forward = merge(local.clone(), vec![p1.clone(), p3.clone()], at());
        let reverse = merge(local, vec![p3, p1], at());

        assert_eq!(forward.nodes, reverse.nodes);
        assert_eq!(forward.workers, reverse.workers);
        assert_eq!(forward.cas, reverse.cas);
        assert_eq!(forward.policies, reverse.policies);
    }

    /// A single node with no peers is not degraded and reports itself.
    #[test]
    fn a_single_node_with_no_peers_is_healthy() {
        let snap = merge(state("solo", RaftRole::Leader, &["w-a"]), vec![], at());
        assert_eq!(snap.nodes.len(), 1);
        assert_eq!(snap.workers.len(), 1);
        assert!(!snap.degraded);
        assert_eq!(snap.leader_id.as_deref(), Some("solo"));
        assert_eq!(snap.as_of, Some(at()));
    }
}
```

- [ ] **Step 2: Run to verify the tests fail**

```bash
cargo test -p brokkr-control --lib cluster::aggregate
```

Expected: FAIL to compile — `merge`, `NodeState`, `PeerOutcome`, `ClusterSnapshot` not found.

- [ ] **Step 3: Implement the types and `merge`**

Prepend to `crates/brokkr-control/src/cluster/aggregate.rs`:

```rust
//! Pure aggregation of per-node observability state into a cluster view.
//!
//! # The rule that matters: not everything can be combined
//!
//! Aggregation is three different operations depending on what is being
//! aggregated, and an implementation that cannot tell them apart produces
//! confident nonsense — which is worse than the separate per-node views
//! fan-out was meant to replace.
//!
//! | Data | Rule |
//! |---|---|
//! | workers, jobs | **union**, each keeping `owning_node` |
//! | CAS stats, policy counters | **one entry per node**, never combined |
//! | leader, term, quorum | **from Raft**, never from counting replies |
//!
//! Each control-plane node opens its own CAS, so summing `objects` across
//! three nodes reports storage that does not exist. Each runs its own policy
//! engine, so two nodes disagreeing about quarantine is information rather
//! than noise.

use std::sync::Arc;
use std::time::SystemTime;

use tokio::sync::RwLock;

use crate::views::{unreachable_node_view, CasStatsView, NodeView, PolicyView, RaftRole, WorkerView};

/// One node's complete observability state — this node's own, or a peer's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeState {
    /// The node itself.
    pub node: NodeView,
    /// Workers in this node's registry.
    pub workers: Vec<WorkerView>,
    /// This node's scheduling-policy state.
    pub policy: PolicyView,
    /// This node's CAS size.
    pub cas: CasStatsView,
}

/// The result of asking one peer for its state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerOutcome {
    /// The peer answered.
    Answered(NodeState),
    /// A known cluster member that did not answer within the deadline.
    ///
    /// Carried rather than dropped so the merged view can show the node as
    /// present-but-silent.
    Unreachable {
        /// The peer's Raft node id.
        node_id: String,
        /// The peer's advertised address.
        advertise_addr: String,
    },
}

/// A cluster-wide view, assembled from every node that answered.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ClusterSnapshot {
    /// Every known node, reachable or not, sorted by id.
    pub nodes: Vec<NodeView>,
    /// Every worker across every node that answered, sorted by id.
    pub workers: Vec<WorkerView>,
    /// One policy view per answering node, sorted by owner.
    pub policies: Vec<PolicyView>,
    /// One CAS view per answering node, sorted by owner. Never summed.
    pub cas: Vec<CasStatsView>,
    /// The node claiming leadership, if exactly one does.
    pub leader_id: Option<String>,
    /// True when any known node was silent, or leadership is ambiguous.
    pub degraded: bool,
    /// When this snapshot was assembled. `None` before the first poll.
    pub as_of: Option<SystemTime>,
}

/// A [`ClusterSnapshot`] behind a lock. The poller is the only writer.
pub type SharedSnapshot = Arc<RwLock<ClusterSnapshot>>;

/// Merge this node's state with its peers' outcomes.
///
/// Deterministic: every output collection is sorted, because peer replies
/// arrive in completion order and an unsorted merge would reorder on every
/// poll.
pub fn merge(local: NodeState, peers: Vec<PeerOutcome>, as_of: SystemTime) -> ClusterSnapshot {
    let mut nodes = vec![local.node.clone()];
    let mut workers = local.workers.clone();
    let mut policies = vec![local.policy.clone()];
    let mut cas = vec![local.cas.clone()];
    let mut any_silent = false;

    for outcome in peers {
        match outcome {
            PeerOutcome::Answered(state) => {
                nodes.push(state.node);
                workers.extend(state.workers);
                policies.push(state.policy);
                cas.push(state.cas);
            }
            PeerOutcome::Unreachable {
                node_id,
                advertise_addr,
            } => {
                any_silent = true;
                nodes.push(unreachable_node_view(&node_id, &advertise_addr));
            }
        }
    }

    nodes.sort_by(|a, b| a.node_id.cmp(&b.node_id));
    workers.sort_by(|a, b| a.worker_id.cmp(&b.worker_id));
    policies.sort_by(|a, b| a.owning_node.cmp(&b.owning_node));
    cas.sort_by(|a, b| a.owning_node.cmp(&b.owning_node));

    // Leadership comes from Raft, never from counting agreement. Exactly one
    // claimant is a healthy cluster; zero means an election is in progress and
    // more than one means a partition. Both are worth surfacing, and picking
    // one arbitrarily would be a confident lie.
    let mut claimants = nodes.iter().filter(|n| n.role == RaftRole::Leader);
    let leader_id = match (claimants.next(), claimants.next()) {
        (Some(only), None) => Some(only.node_id.clone()),
        _ => None,
    };

    ClusterSnapshot {
        nodes,
        workers,
        policies,
        cas,
        degraded: any_silent || leader_id.is_none(),
        leader_id,
        as_of: Some(as_of),
    }
}
```

- [ ] **Step 4: Run the aggregation tests**

```bash
cargo test -p brokkr-control --lib cluster::aggregate
```

Expected: PASS, 9 tests.

- [ ] **Step 5: Write the failing poller tests against a fake probe**

Create `crates/brokkr-control/src/cluster/probe.rs` with only this test module:

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::disallowed_methods, clippy::panic)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;
    use crate::views::{CasStatsView, NodeView, PolicyView, RaftRole};

    fn node_state(id: &str, role: RaftRole) -> NodeState {
        NodeState {
            node: NodeView {
                node_id: id.to_string(),
                advertise_addr: format!("{id}:7878"),
                role,
                term: 1,
                commit_index: 1,
                last_applied: 1,
                reachable: true,
                last_seen_secs: 0,
            },
            workers: Vec::new(),
            policy: PolicyView {
                loaded: false,
                quarantined: false,
                decided: 0,
                declined: 0,
                failures_by_reason: BTreeMap::new(),
                owning_node: id.to_string(),
            },
            cas: CasStatsView {
                objects: 0,
                bytes: 0,
                owning_node: id.to_string(),
            },
        }
    }

    /// A probe whose behaviour is scripted per address.
    struct FakeProbe {
        healthy: Vec<String>,
        slow: Vec<String>,
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl PeerProbe for FakeProbe {
        async fn get_local_state(&self, addr: &str) -> Result<NodeState, ProbeError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            if self.slow.iter().any(|a| a == addr) {
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
            if self.healthy.iter().any(|a| a == addr) {
                let id = addr.split(':').next().unwrap_or(addr);
                return Ok(node_state(id, RaftRole::Follower));
            }
            Err(ProbeError::Unreachable("connection refused".to_string()))
        }
    }

    fn peers() -> Vec<PeerAddr> {
        vec![
            PeerAddr {
                node_id: "node-2".to_string(),
                advertise_addr: "node-2:7878".to_string(),
            },
            PeerAddr {
                node_id: "node-3".to_string(),
                advertise_addr: "node-3:7878".to_string(),
            },
        ]
    }

    #[tokio::test]
    async fn every_healthy_peer_answers() {
        let probe = FakeProbe {
            healthy: vec!["node-2:7878".to_string(), "node-3:7878".to_string()],
            slow: Vec::new(),
            calls: AtomicUsize::new(0),
        };
        let out = poll_peers(&probe, &peers(), Duration::from_millis(500)).await;
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|o| matches!(o, PeerOutcome::Answered(_))));
    }

    /// A refused peer becomes `Unreachable` carrying its identity, not an
    /// error that aborts the round.
    #[tokio::test]
    async fn a_refused_peer_becomes_unreachable_not_an_error() {
        let probe = FakeProbe {
            healthy: vec!["node-2:7878".to_string()],
            slow: Vec::new(),
            calls: AtomicUsize::new(0),
        };
        let out = poll_peers(&probe, &peers(), Duration::from_millis(500)).await;
        assert_eq!(out.len(), 2);
        let unreachable: Vec<&PeerOutcome> = out
            .iter()
            .filter(|o| matches!(o, PeerOutcome::Unreachable { .. }))
            .collect();
        assert_eq!(unreachable.len(), 1);
        match unreachable[0] {
            PeerOutcome::Unreachable {
                node_id,
                advertise_addr,
            } => {
                assert_eq!(node_id, "node-3");
                assert_eq!(advertise_addr, "node-3:7878");
            }
            other => panic!("expected Unreachable, got {other:?}"),
        }
    }

    /// One hung peer must not stall the round. This is the property that keeps
    /// a single wedged node from freezing every operator's console.
    #[tokio::test]
    async fn a_peer_slower_than_the_deadline_is_treated_as_unreachable() {
        let probe = FakeProbe {
            healthy: vec!["node-2:7878".to_string(), "node-3:7878".to_string()],
            slow: vec!["node-3:7878".to_string()],
            calls: AtomicUsize::new(0),
        };
        let started = tokio::time::Instant::now();
        let out = poll_peers(&probe, &peers(), Duration::from_millis(100)).await;
        let elapsed = started.elapsed();

        assert_eq!(out.len(), 2);
        assert!(
            out.iter()
                .any(|o| matches!(o, PeerOutcome::Unreachable { node_id, .. } if node_id == "node-3")),
            "the slow peer should have timed out"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "the round took {elapsed:?}; a hung peer stalled it"
        );
    }

    /// Peers are probed concurrently, so the round costs one deadline rather
    /// than N.
    #[tokio::test]
    async fn peers_are_probed_concurrently() {
        let probe = FakeProbe {
            healthy: Vec::new(),
            slow: vec!["node-2:7878".to_string(), "node-3:7878".to_string()],
            calls: AtomicUsize::new(0),
        };
        let started = tokio::time::Instant::now();
        let _ = poll_peers(&probe, &peers(), Duration::from_millis(200)).await;
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(600),
            "two 200ms deadlines took {elapsed:?}; probes ran serially"
        );
    }
}
```

- [ ] **Step 6: Run to verify they fail, then implement the probe**

```bash
cargo test -p brokkr-control --lib cluster::probe
```

Expected: FAIL to compile — `PeerProbe`, `ProbeError`, `PeerAddr`, `poll_peers` not found.

Prepend to `crates/brokkr-control/src/cluster/probe.rs`:

```rust
//! Asking peers for their observability state.
//!
//! The transport sits behind a trait so the poller's *policy* — deadlines,
//! what counts as unreachable, concurrency — is testable without a socket.
//! That discipline has repeatedly paid off in this codebase (`rotation_plan`,
//! `redirect::classify`, `resolve_raft_tls`, `should_reload`).

use std::time::Duration;

use thiserror::Error;

use super::aggregate::{NodeState, PeerOutcome};

/// A peer's identity and where to reach it, from the Raft cluster config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerAddr {
    /// The peer's Raft node id.
    pub node_id: String,
    /// The peer's advertised address.
    pub advertise_addr: String,
}

/// Why a peer could not be read.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ProbeError {
    /// The peer refused, reset, or could not be resolved.
    #[error("peer unreachable: {0}")]
    Unreachable(String),
    /// The peer answered with something unusable.
    #[error("peer returned an unusable reply: {0}")]
    Malformed(String),
}

/// How the poller reaches a peer.
#[async_trait::async_trait]
pub trait PeerProbe: Send + Sync {
    /// Fetch one peer's node-local state.
    async fn get_local_state(&self, addr: &str) -> Result<NodeState, ProbeError>;
}

/// Probe every peer concurrently, converting any failure into
/// [`PeerOutcome::Unreachable`].
///
/// Never returns an error. A round in which every peer is down is a *degraded
/// cluster*, not a failed observation — and an observability path is most
/// needed exactly then.
///
/// `deadline` is applied per peer, and probes run concurrently, so a round
/// costs one deadline rather than one per peer.
pub async fn poll_peers(
    probe: &dyn PeerProbe,
    peers: &[PeerAddr],
    deadline: Duration,
) -> Vec<PeerOutcome> {
    let futures = peers.iter().map(|peer| async move {
        match tokio::time::timeout(deadline, probe.get_local_state(&peer.advertise_addr)).await {
            Ok(Ok(state)) => PeerOutcome::Answered(state),
            Ok(Err(e)) => {
                tracing::debug!(
                    node_id = %peer.node_id,
                    addr = %peer.advertise_addr,
                    error = %e,
                    "observability peer probe failed"
                );
                PeerOutcome::Unreachable {
                    node_id: peer.node_id.clone(),
                    advertise_addr: peer.advertise_addr.clone(),
                }
            }
            Err(_) => {
                tracing::debug!(
                    node_id = %peer.node_id,
                    addr = %peer.advertise_addr,
                    ?deadline,
                    "observability peer probe timed out"
                );
                PeerOutcome::Unreachable {
                    node_id: peer.node_id.clone(),
                    advertise_addr: peer.advertise_addr.clone(),
                }
            }
        }
    });
    futures::future::join_all(futures).await
}
```

`futures` is already a workspace dependency; no manifest change is needed.

- [ ] **Step 7: Run the probe tests**

```bash
cargo test -p brokkr-control --lib cluster::probe
```

Expected: PASS, 4 tests.

- [ ] **Step 8: Write the poller task and its config**

Create `crates/brokkr-control/src/cluster/mod.rs`:

```rust
//! Cluster-wide observability aggregation (ADR 0012).
//!
//! One task per node polls every Raft peer into a [`ClusterSnapshot`]; every
//! operator-facing handler reads that snapshot. Peer traffic is therefore
//! constant and independent of how many operators are watching — an operator
//! console is exactly the thing left open on a wall display, and per-request
//! fan-out would make an idle dashboard expensive.
//!
//! The cost is bounded, known staleness: nothing here is fresher than one poll
//! interval. `ClusterSnapshot::as_of` carries that so a consumer can show it,
//! and the two events where latency actually matters — leadership change and
//! policy quarantine — bypass the poll entirely.

mod aggregate;
mod probe;

pub use aggregate::{merge, ClusterSnapshot, NodeState, PeerOutcome, SharedSnapshot};
pub use probe::{poll_peers, PeerAddr, PeerProbe, ProbeError};

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tracing::Instrument as _;

/// How the poller is configured.
#[derive(Debug, Clone)]
pub struct PollerConfig {
    /// How often to poll peers. Zero disables fan-out entirely.
    pub interval: Duration,
    /// Per-peer deadline. Must be below `interval`.
    pub peer_timeout: Duration,
    /// How often to re-measure this node's own CAS.
    ///
    /// Slower than `interval` on purpose: `RedbCas` answers by scanning the
    /// blob table under a throughput permit, so measuring it every poll would
    /// be O(n) each time and could take a permit real traffic needs.
    pub cas_interval: Duration,
}

/// Everything the poller needs to build this node's own state and find peers.
///
/// Not `Debug`: it holds trait objects over the registry, scheduler, CAS and
/// policy engine, none of which require `Debug`, and adding that bound to all
/// of them for a log line would be the tail wagging the dog.
pub struct PollerDeps {
    /// Builds this node's own `NodeState`. Async because the CAS read is.
    pub local: Arc<dyn LocalStateSource>,
    /// Current peer set, re-read each round so membership changes are picked
    /// up without a restart.
    pub peers: Arc<dyn PeerDirectory>,
    /// Transport to peers.
    pub probe: Arc<dyn PeerProbe>,
}

/// Produces this node's own observability state.
#[async_trait::async_trait]
pub trait LocalStateSource: Send + Sync {
    /// Build this node's state. `cas` is `None` when the CAS measurement is
    /// being skipped this round, in which case the previous value is reused.
    async fn local_state(&self, refresh_cas: bool) -> NodeState;
}

/// Supplies the current peer set from the Raft cluster configuration.
#[async_trait::async_trait]
pub trait PeerDirectory: Send + Sync {
    /// Peers other than this node. Empty when Raft is disabled.
    async fn peers(&self) -> Vec<PeerAddr>;
}

/// Run the poll loop until the task is dropped.
pub fn spawn_poller(
    shared: SharedSnapshot,
    deps: PollerDeps,
    cfg: PollerConfig,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(
        async move {
            if cfg.interval.is_zero() {
                // Still publish local state once, so a single-node or
                // fan-out-disabled deployment serves something rather than an
                // empty snapshot forever.
                let local = deps.local.local_state(true).await;
                let snapshot = merge(local, Vec::new(), SystemTime::now());
                *shared.write().await = snapshot;
                tracing::info!("observability fan-out disabled; serving node-local state");
                return;
            }

            let mut ticker = tokio::time::interval(cfg.interval);
            let mut since_cas = cfg.cas_interval;
            loop {
                ticker.tick().await;
                let refresh_cas = since_cas >= cfg.cas_interval;
                if refresh_cas {
                    since_cas = Duration::ZERO;
                } else {
                    since_cas += cfg.interval;
                }

                let local = deps.local.local_state(refresh_cas).await;
                let peers = deps.peers.peers().await;
                let outcomes = poll_peers(deps.probe.as_ref(), &peers, cfg.peer_timeout).await;
                let snapshot = merge(local, outcomes, SystemTime::now());

                if snapshot.degraded {
                    tracing::warn!(
                        nodes = snapshot.nodes.len(),
                        reachable = snapshot.nodes.iter().filter(|n| n.reachable).count(),
                        leader = ?snapshot.leader_id,
                        "cluster observability is degraded"
                    );
                }
                *shared.write().await = snapshot;
            }
        }
        .in_current_span(),
    )
}
```

Add `pub mod cluster;` to `crates/brokkr-control/src/lib.rs`, before
`pub mod fairqueue;` to keep the list alphabetical.

- [ ] **Step 9: Add the flags and startup validation**

In `crates/brokkr-control/src/main.rs`, beside `--observe-listen`:

```rust
    /// How often peers are polled for observability state (ADR 0012).
    /// `0` disables fan-out and serves node-local state only.
    #[arg(long, default_value_t = 2)]
    observe_poll_interval_secs: u64,

    /// Per-peer deadline for an observability poll, in milliseconds.
    #[arg(long, default_value_t = 750)]
    observe_peer_timeout_ms: u64,

    /// How often this node re-measures its own CAS size, in seconds.
    ///
    /// Deliberately slower than the peer poll: `RedbCas` answers by scanning
    /// the blob table under a throughput permit, so measuring it every poll
    /// would be O(n) each time and could take a permit real traffic needs.
    #[arg(long, default_value_t = 30)]
    observe_cas_interval_secs: u64,
```

Add a pure validator next to `resolve_raft_tls`, and a test for it:

```rust
/// Reject a peer deadline at or above the poll interval.
///
/// A deadline that can outlast the interval silently serialises the poll loop:
/// each round waits for the previous round's stragglers, and the snapshot ages
/// without anything reporting a problem. Caught at startup rather than
/// discovered as mysterious staleness in production.
fn validate_observe_timing(
    poll_interval_secs: u64,
    peer_timeout_ms: u64,
) -> Result<(), String> {
    if poll_interval_secs == 0 {
        return Ok(()); // fan-out disabled; the deadline is unused
    }
    let interval_ms = poll_interval_secs.saturating_mul(1000);
    if peer_timeout_ms >= interval_ms {
        return Err(format!(
            "--observe-peer-timeout-ms ({peer_timeout_ms}) must be below \
             --observe-poll-interval-secs ({poll_interval_secs} = {interval_ms}ms); \
             a deadline that outlasts the interval serialises the poll loop"
        ));
    }
    Ok(())
}
```

Test it in `main.rs`'s existing test module:

```rust
    #[test]
    fn a_peer_deadline_at_or_above_the_interval_is_rejected() {
        assert!(validate_observe_timing(2, 750).is_ok());
        assert!(validate_observe_timing(2, 2000).is_err());
        assert!(validate_observe_timing(2, 2001).is_err());
        // Fan-out disabled: the deadline is unused, so anything is fine.
        assert!(validate_observe_timing(0, 99_999).is_ok());
    }
```

Call it before spawning the poller and return the error via `anyhow::bail!`.

- [ ] **Step 10: Serve handlers from the snapshot**

Change `ObservabilityService` to hold a `SharedSnapshot` and read it. Preserve
Task 4's behaviour when fan-out is disabled — local-only, one node — which the
`interval.is_zero()` branch of `spawn_poller` already guarantees.

- [ ] **Step 11: Run the full verification gate**

Run the six commands from Global Constraints.

- [ ] **Step 12: Update the CHANGELOG and commit**

Add under `## [Unreleased]` → `### Added`:

```markdown
- **Cluster-wide observability aggregation.** Each node polls its Raft peers
  (`--observe-poll-interval-secs`, default 2) into a `ClusterSnapshot` that
  every handler serves from, so peer traffic is independent of how many
  operators are watching. An unreachable peer marks the snapshot `degraded` and
  keeps the node visible rather than failing the call — an observability API is
  most needed exactly when something is broken. CAS and policy state are
  reported per node and never combined; leadership is taken from Raft rather
  than from counting replies, and both zero and multiple claimants report no
  leader rather than picking one. CAS is re-measured on a slower cadence
  (`--observe-cas-interval-secs`, default 30) because `RedbCas` answers by
  scanning under a throughput permit.
```

```bash
git add crates/brokkr-control CHANGELOG.md
git commit -m "feat(control): aggregate observability state across Raft peers

Motivation: the worker registry is per-node and unreplicated, so ListWorkers
answers differently depending on which node you ask. An operator console that
showed a third of the fleet without saying so would be worse than none.

A background poller rather than per-request fan-out: peer traffic is then
independent of how many operators are watching, which matters because a console
is exactly the thing left open on a wall display. The cost is bounded, known
staleness, carried explicitly as ClusterSnapshot::as_of.

Aggregation is three different operations and conflating them produces
confident nonsense:
- workers union, each keeping owning_node
- CAS and policy are reported per node and NEVER summed. Each node opens its
  own CAS, so one blob on three nodes is three copies; a total would report
  storage that does not exist.
- leadership comes from Raft, not from counting agreement. Zero claimants means
  an election, more than one means a partition; both report no leader and mark
  degraded rather than picking one, because picking one would be a confident
  lie exactly when an operator most needs the truth.

An unreachable peer is carried as Unreachable with its identity rather than
dropped, so 'known but silent' stays distinct from 'not a member'.

The peer deadline is validated below the poll interval at startup. A deadline
that outlasts the interval silently serialises the loop — each round waiting on
the previous round's stragglers — and the snapshot ages with nothing reporting
a problem.

How tested: 9 pure merge tests (union, per-node CAS, per-node policy,
leadership from Raft, unreachable degrades without removing, two claimants, no
claimant, order-independence, single node) and 4 poller tests against a fake
probe (healthy, refused, slower-than-deadline, concurrency), plus a pure
validator test for the timing constraint. No sockets in any of them.

Related: docs/superpowers/specs/2026-08-02-observability-read-model-design.md
D1, D3, D7; ADR 0012."
```

---

### Task 7: Jobs — the history ring and job RPCs

Branch: `feat/job-views` from `origin/main`.

**Files:**
- Create: `crates/brokkr-control/src/views/job.rs`
- Modify: `crates/brokkr-control/src/views/mod.rs`
- Modify: `crates/brokkr-control/src/scheduler.rs`
- Modify: `crates/brokkr-proto/protos/brokkr/v1/observability.proto`
- Modify: `crates/brokkr-control/src/services/observability.rs`
- Modify: `crates/brokkr-control/src/main.rs`
- Modify: `CHANGELOG.md`

**Interfaces:**
- Produces: `views::{JobState, JobSummary, JobHistory}`; proto `JobInfo`, `ListJobsRequest { string state_filter = 1; uint32 limit = 2; }`, `ListJobsReply { repeated JobInfo jobs = 1; }`; `rpc ListJobs`, `rpc GetJob`.

**Why this is its own task, after aggregation:** the ring buffer is written from
`Scheduler::report()`, which is on the dispatch hot path. Isolating it means a
reviewer can weigh that change on its own rather than inside a large PR.

- [ ] **Step 1: Write the failing ring-buffer tests**

Create `crates/brokkr-control/src/views/job.rs` with only this test module:

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::disallowed_methods, clippy::panic)]
mod tests {
    use super::*;

    fn summary(id: &str) -> JobSummary {
        JobSummary {
            job_id: id.to_string(),
            tenant: "t".to_string(),
            action_digest: "a".repeat(64),
            state: JobState::Succeeded,
            worker_id: Some("w-a".to_string()),
            exit_code: Some(0),
            owning_node: "node-1".to_string(),
        }
    }

    #[test]
    fn an_empty_history_lists_nothing() {
        let h = JobHistory::new(4);
        assert!(h.recent(10).is_empty());
    }

    /// Newest first: an operator opening a console wants the last thing that
    /// happened, not the oldest thing still retained.
    #[test]
    fn recent_returns_newest_first() {
        let mut h = JobHistory::new(8);
        for id in ["j1", "j2", "j3"] {
            h.record(summary(id));
        }
        let ids: Vec<&str> = h.recent(10).iter().map(|j| j.job_id.as_str()).collect();
        assert_eq!(ids, vec!["j3", "j2", "j1"]);
    }

    #[test]
    fn the_ring_is_bounded_and_drops_oldest_first() {
        let mut h = JobHistory::new(3);
        for id in ["j1", "j2", "j3", "j4", "j5"] {
            h.record(summary(id));
        }
        let ids: Vec<&str> = h.recent(10).iter().map(|j| j.job_id.as_str()).collect();
        assert_eq!(ids, vec!["j5", "j4", "j3"]);
    }

    #[test]
    fn recent_respects_its_limit() {
        let mut h = JobHistory::new(8);
        for id in ["j1", "j2", "j3", "j4"] {
            h.record(summary(id));
        }
        assert_eq!(h.recent(2).len(), 2);
        assert_eq!(h.recent(0).len(), 0);
    }

    /// A capacity of zero would silently record nothing while still costing
    /// the call on every report — a configuration mistake, not a way to
    /// disable the feature.
    #[test]
    fn a_zero_capacity_is_clamped() {
        let mut h = JobHistory::new(0);
        h.record(summary("j1"));
        assert_eq!(h.recent(10).len(), 1);
    }

    #[test]
    fn state_filter_selects_only_matching_jobs() {
        let mut h = JobHistory::new(8);
        h.record(summary("ok"));
        let mut failed = summary("bad");
        failed.state = JobState::Failed;
        failed.exit_code = Some(1);
        h.record(failed);

        let only_failed = h.filtered(Some(JobState::Failed), 10);
        assert_eq!(only_failed.len(), 1);
        assert_eq!(only_failed[0].job_id, "bad");
        assert_eq!(h.filtered(None, 10).len(), 2);
    }
}
```

- [ ] **Step 2: Run to verify it fails, then implement**

```bash
cargo test -p brokkr-control --lib views::job
```

Prepend to `crates/brokkr-control/src/views/job.rs`:

```rust
//! Job projections and the bounded completed-job history.

use std::collections::VecDeque;

/// What happened to a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    /// Waiting for an eligible, idle worker.
    Queued,
    /// Leased to a worker and running.
    Running,
    /// Reported a zero exit code.
    Succeeded,
    /// Reported a non-zero exit code, or failed to dispatch.
    Failed,
}

/// One job, as an operator sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobSummary {
    /// Server-assigned job id.
    pub job_id: String,
    /// Submitting tenant.
    pub tenant: String,
    /// Lowercase hex sha256 of the action.
    pub action_digest: String,
    /// The job's state.
    pub state: JobState,
    /// The worker that ran it, once one was chosen.
    pub worker_id: Option<String>,
    /// Exit code, once reported.
    pub exit_code: Option<i32>,
    /// The control-plane node that scheduled this job.
    ///
    /// Per node like every other node-local record: the history ring lives in
    /// one node's memory and is never a cluster-wide fact.
    pub owning_node: String,
}

/// A bounded ring of recently completed jobs.
///
/// In-memory and bounded on purpose — durable job history is a
/// scheduler-storage decision ADR 0012 explicitly defers.
#[derive(Debug)]
pub struct JobHistory {
    entries: VecDeque<JobSummary>,
    capacity: usize,
}

impl JobHistory {
    /// A history retaining `capacity` completed jobs.
    ///
    /// Clamped to at least 1: a zero capacity would record nothing while still
    /// costing the call on every report, which is a configuration mistake
    /// rather than a way to disable the feature.
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            capacity: capacity.max(1),
        }
    }

    /// Record a completed job, evicting the oldest if at capacity.
    pub fn record(&mut self, summary: JobSummary) {
        if self.entries.len() == self.capacity {
            self.entries.pop_front();
        }
        self.entries.push_back(summary);
    }

    /// The most recent `limit` jobs, newest first.
    pub fn recent(&self, limit: usize) -> Vec<JobSummary> {
        self.entries.iter().rev().take(limit).cloned().collect()
    }

    /// The most recent `limit` jobs matching `state`, newest first.
    pub fn filtered(&self, state: Option<JobState>, limit: usize) -> Vec<JobSummary> {
        self.entries
            .iter()
            .rev()
            .filter(|j| state.is_none_or(|s| j.state == s))
            .take(limit)
            .cloned()
            .collect()
    }
}
```

- [ ] **Step 3: Run the tests**

```bash
cargo test -p brokkr-control --lib views::job
```

Expected: PASS, 6 tests.

- [ ] **Step 4: Populate the ring from `Scheduler::report()`**

Add `job_history: JobHistory` to `Inner` beside `locality`, and record in
`report()` in the same block that already records locality — the worker and the
`PendingJob` are both in hand there, so this adds no lookups:

```rust
            if let (Some(w), Some(pj)) = (worker, completed.as_ref()) {
                inner
                    .locality
                    .record(&w, &pj.action_digest, pj.input_root_digest.as_ref());
                inner.job_history.record(JobSummary {
                    job_id: pj.job_id.as_str().to_string(),
                    tenant: pj.tenant.as_str().to_string(),
                    action_digest: pj.action_digest.hash().to_string(),
                    state: match result.result.as_ref().map(|r| r.exit_code) {
                        Some(0) => JobState::Succeeded,
                        _ => JobState::Failed,
                    },
                    worker_id: Some(w.as_str().to_string()),
                    exit_code: result.result.as_ref().map(|r| r.exit_code),
                    owning_node: self.node_id.clone(),
                });
            }
```

`Scheduler` gains a `node_id: String` field, defaulting to `"local"` in the
existing constructors and set from `--node-id` in `main.rs`. Add a scheduler
test asserting a reported job appears in the history with the right state, and
that a non-zero exit code records as `Failed`.

- [ ] **Step 5: Add the flag, the proto, and the RPCs**

```rust
    /// Completed jobs retained per node for the observability history.
    #[arg(long, default_value_t = 256)]
    observe_job_history: usize,
```

Add to `observability.proto`:

```proto
message JobInfo {
  string job_id = 1;
  string tenant = 2;
  string action_digest = 3;
  // "queued" | "running" | "succeeded" | "failed"
  string state = 4;
  string worker_id = 5;
  int32 exit_code = 6;
  bool has_exit_code = 7;
  string owning_node = 8;
}

message ListJobsRequest {
  // Empty means no filter.
  string state_filter = 1;
  // Zero means the server default.
  uint32 limit = 2;
}
message ListJobsReply { repeated JobInfo jobs = 1; }

message GetJobRequest { string job_id = 1; }
message GetJobReply { JobInfo job = 1; }
```

Add `rpc ListJobs(ListJobsRequest) returns (ListJobsReply);` and
`rpc GetJob(GetJobRequest) returns (GetJobReply);` to `ObservabilityService`,
and `repeated JobInfo jobs = 5;` to `GetLocalStateReply` in `raft.proto` so
jobs aggregate like workers. `has_exit_code` exists because proto3 cannot
distinguish an unset `int32` from `0`, and `0` is a meaningful exit code.

- [ ] **Step 6: Extend `merge` to union jobs**

Add `jobs: Vec<JobSummary>` to `NodeState` and `ClusterSnapshot`, unioned and
sorted like workers. Add an aggregation test asserting jobs from all nodes
appear with their `owning_node` preserved.

- [ ] **Step 7: Run the gate, CHANGELOG, commit**

Add under `## [Unreleased]` → `### Added`:

```markdown
- **Job history and `ListJobs` / `GetJob`.** A bounded in-memory ring of the
  last `--observe-job-history` (default 256) completed jobs per node,
  populated in `Scheduler::report()` alongside the locality record so it costs
  no extra lookups. Durable history stays deferred per ADR 0012. `JobInfo`
  carries `has_exit_code` because proto3 cannot distinguish an unset `int32`
  from `0`, and `0` is a meaningful exit code.
```

---

### Task 8: `WatchEvents`

Branch: `feat/watch-events` from `origin/main`.

**Files:**
- Create: `crates/brokkr-control/src/cluster/events.rs`
- Modify: `crates/brokkr-control/src/cluster/mod.rs`
- Modify: `crates/brokkr-proto/protos/brokkr/v1/observability.proto`
- Modify: `crates/brokkr-control/src/services/observability.rs`
- Modify: `CHANGELOG.md`

**Interfaces:**
- Produces: `cluster::events::{ClusterEvent, diff}`; proto `WatchEventsRequest`, `ClusterEvent`; `rpc WatchEvents(WatchEventsRequest) returns (stream ClusterEvent);`

- [ ] **Step 1: Write the failing diff tests**

Create `crates/brokkr-control/src/cluster/events.rs` with only this test module.
Duplicate the fixture helpers from `aggregate.rs` locally rather than exporting
test helpers across modules.

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::disallowed_methods, clippy::panic)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::{Duration, SystemTime};

    use super::*;
    use crate::cluster::ClusterSnapshot;
    use crate::views::{CasStatsView, NodeView, PolicyView, RaftRole, WorkerView};

    fn at() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
    }

    fn node(id: &str, role: RaftRole, reachable: bool) -> NodeView {
        NodeView {
            node_id: id.to_string(),
            advertise_addr: format!("{id}:7878"),
            role,
            term: 7,
            commit_index: 42,
            last_applied: 42,
            reachable,
            last_seen_secs: 0,
        }
    }

    fn worker(id: &str, owner: &str, stale: bool) -> WorkerView {
        WorkerView {
            worker_id: id.to_string(),
            hostname: id.to_string(),
            labels: BTreeMap::new(),
            inflight: 0,
            last_seen_secs: 1,
            stale,
            owning_node: owner.to_string(),
        }
    }

    fn policy(owner: &str, quarantined: bool) -> PolicyView {
        PolicyView {
            loaded: true,
            quarantined,
            decided: 0,
            declined: 0,
            failures_by_reason: BTreeMap::new(),
            owning_node: owner.to_string(),
        }
    }

    /// A snapshot with one node, the given workers, and the given policy.
    fn snap(
        nodes: Vec<NodeView>,
        workers: Vec<WorkerView>,
        policies: Vec<PolicyView>,
        leader: Option<&str>,
    ) -> ClusterSnapshot {
        ClusterSnapshot {
            nodes,
            workers,
            policies,
            cas: vec![CasStatsView {
                objects: 0,
                bytes: 0,
                owning_node: "node-1".to_string(),
            }],
            jobs: Vec::new(),
            leader_id: leader.map(|s| s.to_string()),
            degraded: false,
            as_of: Some(at()),
        }
    }

    fn base() -> ClusterSnapshot {
        snap(
            vec![node("node-1", RaftRole::Leader, true)],
            vec![worker("w-a", "node-1", false)],
            vec![policy("node-1", false)],
            Some("node-1"),
        )
    }

    /// Two identical snapshots produce nothing. Without this a 2s poller
    /// re-emits the entire world every tick and the event stream is useless
    /// noise.
    #[test]
    fn identical_snapshots_produce_no_events() {
        assert!(diff(&base(), &base()).is_empty());
    }

    #[test]
    fn a_new_worker_produces_worker_added() {
        let mut next = base();
        next.workers.push(worker("w-b", "node-1", false));
        next.workers.sort_by(|a, b| a.worker_id.cmp(&b.worker_id));
        assert_eq!(
            diff(&base(), &next),
            vec![ClusterEvent::WorkerAdded {
                worker_id: "w-b".to_string(),
                owning_node: "node-1".to_string(),
            }]
        );
    }

    #[test]
    fn a_missing_worker_produces_worker_removed() {
        let mut next = base();
        next.workers.clear();
        assert_eq!(
            diff(&base(), &next),
            vec![ClusterEvent::WorkerRemoved {
                worker_id: "w-a".to_string(),
                owning_node: "node-1".to_string(),
            }]
        );
    }

    #[test]
    fn a_worker_going_stale_produces_worker_stale() {
        let mut next = base();
        next.workers[0].stale = true;
        assert_eq!(
            diff(&base(), &next),
            vec![ClusterEvent::WorkerStale {
                worker_id: "w-a".to_string(),
                owning_node: "node-1".to_string(),
            }]
        );
    }

    #[test]
    fn a_node_becoming_unreachable_produces_node_unreachable() {
        let prev = snap(
            vec![
                node("node-1", RaftRole::Leader, true),
                node("node-2", RaftRole::Follower, true),
            ],
            Vec::new(),
            Vec::new(),
            Some("node-1"),
        );
        let mut next = prev.clone();
        next.nodes[1].reachable = false;
        assert_eq!(
            diff(&prev, &next),
            vec![ClusterEvent::NodeUnreachable {
                node_id: "node-2".to_string(),
            }]
        );
    }

    #[test]
    fn a_node_recovering_produces_node_recovered() {
        let mut prev = snap(
            vec![
                node("node-1", RaftRole::Leader, true),
                node("node-2", RaftRole::Follower, true),
            ],
            Vec::new(),
            Vec::new(),
            Some("node-1"),
        );
        prev.nodes[1].reachable = false;
        let mut next = prev.clone();
        next.nodes[1].reachable = true;
        assert_eq!(
            diff(&prev, &next),
            vec![ClusterEvent::NodeRecovered {
                node_id: "node-2".to_string(),
            }]
        );
    }

    #[test]
    fn a_leadership_change_produces_leader_changed() {
        let prev = base();
        let mut next = base();
        next.nodes[0].role = RaftRole::Follower;
        next.leader_id = Some("node-2".to_string());
        assert!(next
            .nodes
            .iter()
            .all(|n| n.role != RaftRole::Leader));
        assert_eq!(
            diff(&prev, &next),
            vec![ClusterEvent::LeaderChanged {
                from: Some("node-1".to_string()),
                to: Some("node-2".to_string()),
            }]
        );
    }

    /// Losing the leader entirely is a leadership change too, and is the case
    /// an operator most needs pushed at them.
    #[test]
    fn losing_the_leader_produces_leader_changed_to_none() {
        let prev = base();
        let mut next = base();
        next.nodes[0].role = RaftRole::Unknown;
        next.leader_id = None;
        assert_eq!(
            diff(&prev, &next),
            vec![ClusterEvent::LeaderChanged {
                from: Some("node-1".to_string()),
                to: None,
            }]
        );
    }

    #[test]
    fn a_policy_becoming_quarantined_produces_policy_quarantined() {
        let prev = base();
        let mut next = base();
        next.policies[0].quarantined = true;
        assert_eq!(
            diff(&prev, &next),
            vec![ClusterEvent::PolicyQuarantined {
                owning_node: "node-1".to_string(),
            }]
        );
    }

    /// A policy recovering after a reload is worth reporting too — otherwise
    /// a console shows a quarantine banner that never clears.
    #[test]
    fn a_policy_leaving_quarantine_produces_policy_recovered() {
        let mut prev = base();
        prev.policies[0].quarantined = true;
        let next = base();
        assert_eq!(
            diff(&prev, &next),
            vec![ClusterEvent::PolicyRecovered {
                owning_node: "node-1".to_string(),
            }]
        );
    }

    /// Events are emitted in a fixed order — nodes, workers, policy,
    /// leadership — so two identical transitions produce identical streams.
    #[test]
    fn events_are_ordered_deterministically() {
        let prev = snap(
            vec![
                node("node-1", RaftRole::Leader, true),
                node("node-2", RaftRole::Follower, true),
            ],
            vec![worker("w-a", "node-1", false)],
            vec![policy("node-1", false)],
            Some("node-1"),
        );
        let mut next = prev.clone();
        next.nodes[1].reachable = false;
        next.workers.clear();
        next.policies[0].quarantined = true;
        next.nodes[0].role = RaftRole::Unknown;
        next.leader_id = None;

        let events = diff(&prev, &next);
        let kinds: Vec<&str> = events.iter().map(ClusterEvent::kind).collect();
        assert_eq!(
            kinds,
            vec![
                "node_unreachable",
                "worker_removed",
                "policy_quarantined",
                "leader_changed"
            ],
            "events must be emitted nodes -> workers -> policy -> leadership"
        );
        // And the same transition twice yields the same stream.
        assert_eq!(diff(&prev, &next), events);
    }
}
```

`ClusterEvent::kind` returns a stable lowercase tag per variant, mirroring
`PolicyFailure::reason` from Phase 6 — a dashboard groups on it, so it must be
free of interpolated detail.

- [ ] **Step 2: Implement `diff` as a pure function**

`ClusterEvent` is an enum with one variant per case above, each carrying the
ids involved. `diff(prev, next) -> Vec<ClusterEvent>` compares sorted
collections and emits in a fixed order: nodes, then workers, then policy, then
leadership.

- [ ] **Step 3: Wire the broadcast and the stream**

The poller publishes `diff(prev, next)` after each snapshot swap on a
`tokio::sync::broadcast` channel with capacity 256.

`WatchEvents` first sends a synthetic initial event set describing current
state, so a newly connected client is not blank until something changes, then
forwards the broadcast.

Handle `RecvError::Lagged(n)` by emitting a `Resync` event rather than dropping
the client. A slow operator terminal must not silently miss events — silently
is the problem, not slowly.

Publish leadership change and policy quarantine **immediately** on transition
rather than waiting for the next diff (spec D6): both are incident signals
where a 2s delay matters.

- [ ] **Step 4: Run the gate, CHANGELOG, commit**

---

### Task 9: SDK read client

Branch: `feat/sdk-observability` from `origin/main`.

**Files:**
- Create: `crates/brokkr-sdk/src/observability.rs`
- Modify: `crates/brokkr-sdk/src/lib.rs`
- Modify: `CHANGELOG.md`

**Interfaces:**
- Produces, relied on by the TUI plan:
  - `ObservabilityClient::connect(endpoint: String) -> Result<Self, ClientError>`
  - `get_cluster() -> Result<ClusterInfo, ClientError>`
  - `list_workers() -> Result<Vec<WorkerInfo>, ClientError>`
  - `list_jobs(state: Option<&str>, limit: u32) -> Result<Vec<JobInfo>, ClientError>`
  - `get_policy() -> Result<Vec<PolicyInfo>, ClientError>`
  - `get_cas_stats() -> Result<Vec<CasInfo>, ClientError>`
  - `watch_events() -> Result<impl Stream<Item = Result<ClusterEvent, ClientError>>, ClientError>`

- [ ] **Step 1: Write the failing test**

Add to `crates/brokkr-sdk/tests/` an integration test that boots a control plane
in-process (reuse the harness from Task 4) and walks every method, asserting a
single-node cluster reports one node, itself as leader, and not degraded.

- [ ] **Step 2: Implement**

Follow `crates/brokkr-sdk/src/client.rs` for the connect/TLS idiom exactly.

`brokkr-sdk` is a **library crate**: no `unwrap`/`expect`. Errors go through the
existing `ClientError` enum — add a variant only if none fits.
`#![deny(missing_docs)]` at the top of `lib.rs` means every public item needs a
doc comment, including struct fields.

- [ ] **Step 3: Run the gate, CHANGELOG, commit**

---

## Definition of done for this plan

From the spec, the subset this plan covers:

1. A 3-node cluster shows all workers across all nodes from any node, each labelled with the node that knows about it. *(Task 6)*
2. Killing one node leaves the API working, reporting degraded and marking the dead node unreachable. *(Task 6)*
3. `GetCluster` shows Raft role, term, commit index and leader; a leadership change reaches a `WatchEvents` client without waiting for a poll. *(Tasks 3, 8)*
4. Phase 6's policy counters are visible, including quarantine state. *(Tasks 1, 4)*
5. `ObservabilityService` is unreachable on the tenant-facing listener, proven by test. *(Task 4)*
7. Single-node (`--raft` off) works with no poller and no peer traffic. *(Tasks 4, 6)*

DoD 6 (TUI panic restore) belongs to the TUI plan. The 3-node integration test
proving DoD 1 and 2 end to end also belongs there, alongside the other
`#[ignore]`d multi-node tests; Task 6's fake-probe tests prove the same
behaviour at the unit level here.

## Out of scope for this plan

- The TUI (W8–W10) — its own plan, written once this lands.
- `GetWorker` — trivially derivable from `ListWorkers` client-side, and adding
  an RPC for it before anything needs it is speculative.
- Everything in the spec's own "Out of scope" section.
