# Observability read-model + operator TUI — design

**Date:** 2026-08-02
**Implements:** [ADR 0012](../../architecture/0012-operator-tui.md) (accepted 2026-06-30, unimplemented)
**Status:** design approved, unstarted

---

## Why now, and what changed under the ADR

ADR 0012 decided to build a read-only operator TUI (`brokkr-tui`) fed by a gRPC
`ObservabilityService` over a shared `brokkr-control::views` read-model. It
defined four workstreams (WS0–WS3) and none of them were built.

**The ADR was written on 2026-06-30, before Phase 5 and Phase 6 landed.** Three
things are true now that were not true then, and each changes the design:

1. **The control plane is HA.** A cluster is 3 or 5 Raft nodes, and the worker
   registry is **per-node and unreplicated** — `docs/operations/running-a-cluster.md`
   line 242 states this as a requirement, not an accident. So `ListWorkers`
   answers differently depending on which node you ask. An operator console
   whose entire purpose is "see what Brokkr is doing" showing a third of the
   fleet without saying so would be worse than no console at all.
2. **Raft state exists and is not in the ADR's DTO list.** Who is leader, at what
   term, with what commit index, and whether quorum is healthy is arguably the
   single most valuable thing to observe in an HA cluster. `ClusterInfo`,
   `WorkerView`, `JobSummary`, `JobDetail`, `CasStats` do not cover it.
3. **Phase 6 shipped counters nothing can read.** `WasmStrategy` tracks
   per-reason policy failures, decisions, declines, and quarantine state. That
   was deliberately deferred to this ADR, and is the concrete new consumer that
   makes this work worth doing now.

**The ADR's decisions are not re-litigated here.** Crate layout, gRPC over
REST/SSE, hand-rolled TEA over `tui-realm`, the bounded in-memory job-history
ring, and the dependency set are all settled. This spec covers only what the ADR
could not have decided.

---

## Decisions taken in this design

| # | Decision | Rejected | Why |
|---|---|---|---|
| D1 | **Fan-out and aggregate.** A queried node asks its peers and merges into a cluster-wide view. | Node-local-but-labelled; leader-only; replicating the registry through Raft | Node-local is honest but leaves the operator to mentally union three consoles. Leader-only is actively wrong — the leader's registry is not a superset. Replicating the registry is the right long-term answer and far beyond an observability slice. |
| D2 | **Fan-out rides the Raft peer plane.** | Client plane forwarding the caller's token; client plane with a node service credential | Peers are already mutually authenticated by mTLS there and their addresses are already published (`cfg/nodes/`, `RaftKv::published_addr`). Token forwarding makes the control plane a token relay a compromised node could replay. A node credential is a fourth auth story for one read path. |
| D3 | **A background poller, not per-request fan-out.** One task per node polls peers into a `ClusterSnapshot`; every handler serves from it. | Per-request fan-out; hybrid cache with on-demand refresh | Decouples peer load from observer count — an operator console is exactly the thing left open on a wall display. Makes `WatchEvents` a broadcast of deltas rather than a merge of N upstream streams with N reconnect behaviours. Cost: you cannot see a change faster than the poll interval, which is why D6 exists. |
| D4 | **A separate operator listener, default localhost.** | Same interceptor unrestricted; a new operator scope claim; same interceptor with tenant filtering | ADR 0011's auth has **no scope concept** — `Authenticator::authenticate` returns a `TenantId` and nothing else. Behind the existing interceptor unchanged, any tenant token could enumerate every worker and every other tenant's jobs, a regression against ADR 0010. A separate listener matches the ADR's own stated "internal-only posture (bind localhost / mTLS)" without inventing a scope system for one service. |
| D5 | **A separate peer RPC that structurally cannot recurse.** Fan-out targets `PeerObservability.GetLocalState`, which contains no fan-out code path. | A "don't fan out" flag on `ObservabilityService` | A flag can be forgotten, mis-defaulted, or spoofed. A service with no recursion path cannot be made to recurse. It also inherits peer-plane mTLS and keeps aggregation off the tenant-facing surface entirely. |
| D6 | **Two events bypass the poll interval:** leadership change and policy quarantine. | Everything waits for the next poll | Worker liveness at 2s granularity is fine — the heartbeat deadline is already 15s. A leadership change an operator learns about 2s late during an incident is not fine, and quarantine is the signal that placement quality just silently dropped. |
| D7 | **Partial failure degrades, never fails.** An unreachable peer marks its `NodeView` and sets `degraded`; the call succeeds. | Fail the RPC when any peer is unreachable | An observability API is most needed exactly when something is broken. Failing wholesale because one node is down inverts that. |

---

## Architecture

Five pieces. The crate graph stays a DAG; `brokkr-control` gains no new inbound
edges.

```
brokkr-tui ──▶ brokkr-sdk ──▶ brokkr-proto
brokkr-control ──▶ { brokkr-raft, brokkr-cas, brokkr-policy, brokkr-proto }
```

| Piece | Location | Responsibility |
|---|---|---|
| `views` | `brokkr-control::views` | DTOs and pure projections over local state |
| `cluster` | `brokkr-control::cluster` | the background poller and `ClusterSnapshot` |
| `PeerObservability` | Raft peer plane | node-local state only, for peers |
| `ObservabilityService` | new operator listener | serves the snapshot to operators |
| `brokkr-tui` | new binary crate | `ratatui` + hand-rolled TEA |

### Data flow

```
operator ──▶ TUI ──▶ SDK ──▶ ObservabilityService   (operator listener, localhost)
                                      │
                                      ▼
                              ClusterSnapshot  ◀── poller ──▶ PeerObservability
                                      ▲                        (Raft plane, mTLS)
                                      └── local views
```

The poller is the only writer; every handler is a reader. `WatchEvents` is a
`tokio::broadcast` of deltas computed by diffing successive snapshots, plus the
two immediate events from D6.

---

## Components

### `brokkr-control::views`

Pure projections. No I/O beyond reading the handles it is given, so every
function is unit-testable against fakes.

```
ClusterInfo  { nodes[], leader_id, quorum_healthy, degraded,
               as_of: Option<Timestamp> }   // unset until the first poll lands
NodeView     { node_id, advertise_addr, raft_role, term, commit_index,
               reachable, last_seen }
WorkerView   { worker_id, hostname, labels, liveness, inflight, owning_node }
JobSummary   { job_id, tenant, action_digest, state, worker, timestamps,
               owning_node }
JobDetail    { summary + exit_code, stdout_digest, stderr_digest, attempts }
CasStats     { objects, bytes, dedup_ratio, owning_node }
PolicyView   { loaded, path, quarantined, decided, declined,
               failures_by_reason[], owning_node }
```

`owning_node` is the honesty mechanism, and it appears on **every DTO sourced
from node-local state** — workers, jobs, CAS stats, and policy counters. All of
them are per-node, so aggregation must never present them as a single cluster
fact. Two nodes reporting different policy quarantine states is real
information, not a glitch to average away.

Internal state types must not leak across this boundary — the same rule the ADR
sets and the web spec assumes.

### What aggregation must never do: sum

Each control-plane node opens its **own** CAS (`RedbCas::open(data_dir/cas.redb)`,
`main.rs:610`). Three nodes holding the same blob is three copies of one blob,
not three blobs. Summing `objects` or `bytes` across nodes would report storage
that does not exist, and a dedup ratio that means nothing.

The rule, stated once and applying to every DTO:

- **Union with labels** — workers, jobs. Distinct entities owned by one node
  each; the union is the cluster's real set.
- **Report per node, never combine** — CAS stats, policy counters. Each is a
  local measurement of a local thing. The UI shows them side by side.
- **Derive from Raft, not from summing** — leader, term, quorum health. There is
  one true answer and Raft already knows it.

An aggregation layer that cannot tell these apart produces confident nonsense,
which is worse than the three separate consoles fan-out was meant to replace.

**Job history.** As the ADR specifies: a bounded in-memory ring of the last *N*
completed jobs, populated in `Scheduler::report()`. Default *N* = 256,
configurable. Durable history stays deferred. Note the ring is **per node**, so
it aggregates the same way workers do and carries the same `owning_node`
labelling.

### `brokkr-control::cluster`

Owns `ClusterSnapshot` behind an `RwLock`, and the task that refreshes it.

- Polls every peer on `--observe-poll-interval` (default **2s**).
- Per-peer deadline strictly below the interval, so one hung node cannot stall
  the snapshot.
- Peer addresses come from the Raft config (`cfg/nodes/`), already maintained by
  Phase 5 — no new discovery mechanism.
- Disabled entirely when `--raft` is off. Single-node operators pay nothing and
  the snapshot is local-only with one node.
- Marks unreachable peers rather than dropping them, so "a node I know about is
  not answering" is distinguishable from "that node does not exist".

The poller's *policy* — which peers to ask, what counts as degraded, what
constitutes a delta — is factored into pure functions. This follows what has
worked repeatedly in this codebase (`rotation_plan`, `redirect::classify`,
`resolve_raft_tls`, `should_reload`): the decisions are testable without a
socket.

### `PeerObservability` (Raft peer plane)

One RPC: `GetLocalState` → this node's `views` output and nothing else. No
fan-out path exists in this service. mTLS mutual auth is inherited from the peer
plane.

### `ObservabilityService` (operator listener)

Per the ADR: unary `GetCluster`, `ListWorkers`, `GetWorker`, `ListJobs` (state
filter + limit), `GetJob`, `GetCasStats`, plus server-streaming `WatchEvents`.
Extended with `GetPolicy` for the Phase 6 counters.

Mounted on its own bind address, `--observe-listen`, defaulting to
`127.0.0.1`. Never on the tenant-facing port.

### `brokkr-tui`

`ratatui` + `crossterm`, hand-rolled TEA: `Model` + `Action` + pure
`update(&mut Model, Action)` + `view(&Model, Frame)`. A `tokio::mpsc` action bus
merges `crossterm::EventStream`, a render tick, and the `WatchEvents` stream.

Terminal lifecycle uses a panic hook that restores the terminal before
unwinding — the failure naive ratatui apps get wrong.

MVP panels: Cluster (including Raft roles and quorum health), Workers, Jobs
(live + recent), Policy, Logs via `tui-logger`.

---

## Configuration

| Flag | Default | Purpose |
|---|---|---|
| `--observe-listen` | `127.0.0.1:7880` | Operator listener bind address. Deliberately not the tenant-facing port (D4). |
| `--observe-poll-interval-secs` | `2` | Peer poll cadence (D3). `0` disables fan-out, serving local-only. |
| `--observe-peer-timeout-ms` | `750` | Per-peer deadline. Must stay below the poll interval so one hung node cannot stall a round. |
| `--observe-job-history` | `256` | Bounded completed-job ring per node. |

---

## Error handling

Every case degrades rather than fails.

| Case | Behaviour |
|---|---|
| Peer unreachable | `NodeView.reachable = false`, `ClusterInfo.degraded = true`, call succeeds. Header shows "2 of 3 nodes reporting". |
| Peer slow | Per-peer deadline below the poll interval; that peer is treated as unreachable for this round. |
| No `--raft` | Poller never starts; snapshot is local-only, one node. |
| Snapshot not yet populated | Serve local-only with `as_of` unset rather than blocking startup. |
| TUI panics | Panic hook restores the terminal first. |
| TUI loses its connection | Banner plus backoff retry, not exit. |

---

## Testing

| Layer | Approach |
|---|---|
| `views` | pure functions over registry / scheduler / CAS fakes |
| poller | fake peer client covering unreachable, slow, and **disagreeing** peers |
| snapshot → events | pure diff function, unit tested |
| `update()` | pure TEA reducer, no terminal required |
| render | `ratatui::TestBackend` buffer snapshots |
| integration | in-process boot, walk every RPC |
| multi-node | 3-node aggregation and partial failure, `#[ignore]`d like `raft_ha_e2e` |

Two properties get explicit tests because they are the ones most likely to rot:

- **`ObservabilityService` is not reachable on the tenant-facing listener.** D4's
  entire security argument rests on this.
- **`PeerObservability` does not fan out.** D5's guarantee is structural, and a
  test pins it so a future refactor cannot quietly add recursion.

---

## Sequencing

Each row is one PR, mergeable and green on its own. The order front-loads the
risk: fan-out and its failure modes land before any UI exists to depend on them.

| # | Increment | Depends on | DoD |
|---|-----------|------------|-----|
| W1 | `views` DTOs + pure projections over registry / scheduler / CAS / policy, with `owning_node` throughout. Job-history ring in `Scheduler::report()`. | — | — |
| W2 | Raft state in `views`: role, term, commit index, leader. Single-node correct. | W1 | 3 (partly) |
| W3 | `brokkr/v1/observability.proto` + `ObservabilityService` on its own listener, serving **local-only**. Test that it is unreachable on the tenant port. | W2 | 5, 7 |
| W4 | `PeerObservability.GetLocalState` on the Raft peer plane. No fan-out path; a test pins that. | W3 | — |
| W5 | `cluster` poller + `ClusterSnapshot` + aggregation rules. Fake-peer tests for unreachable, slow, and disagreeing peers. | W4 | 1, 2 |
| W6 | `WatchEvents`: snapshot diffing + the two immediate events. | W5 | 3 |
| W7 | SDK read methods + `watch_events()` wrapper. | W6 | — |
| W8 | `brokkr-tui` scaffold: terminal lifecycle, panic-restore, TEA skeleton, action bus, `Component`. No backend calls. | — | 6 |
| W9 | MVP panels wired to the SDK: Cluster, Workers, Jobs, Policy, Logs. | W7, W8 | 1, 2, 4 |
| W10 | 3-node aggregation + partial-failure integration test (`#[ignore]`d). Docs. | W9 | 1, 2 |

W8 has no backend dependency and can land at any point — useful if the backend
work stalls on something.

---

## Definition of done

1. A 3-node cluster shows all workers across all nodes from any node, each
   labelled with the node that knows about it.
2. Killing one node leaves the console working, showing "2 of 3 nodes reporting"
   and marking the dead node unreachable.
3. `GetCluster` shows Raft role, term, commit index and leader; a leadership
   change reaches a connected `WatchEvents` client without waiting for a poll.
4. Phase 6's policy counters are visible, including quarantine state.
5. `ObservabilityService` is unreachable on the tenant-facing listener, proven by
   test.
6. The TUI's panic hook restores the terminal. Tested at the seam rather than
   by panicking a real terminal: the restore routine is a plain function, unit
   tested directly, and a separate test asserts the hook is installed and
   invokes it. Verified once by hand against a real terminal, and that
   verification is recorded rather than automated.
7. Single-node (`--raft` off) works with no poller and no peer traffic.

---

## Out of scope

Named explicitly, so they are decisions rather than omissions:

- **All mutating actions.** Read-only, as the ADR specifies.
- **Durable job history.** The ring stays in-memory and bounded.
- **The web gateway.** A later `axum` gateway wraps the same `views` functions;
  this spec does not build it.
- **A scope/role system for ADR 0011.** D4 sidesteps it deliberately. If scopes
  arrive later for other reasons, observability can move behind them.
- **V1 panels from the ADR:** job detail with live log tail, digest inspector
  tree (`tui-tree-widget`), replication ring.
- **Replicating the worker registry through Raft.** The real fix for per-node
  registries, and a scheduler-architecture decision.

---

## References

- [ADR 0012](../../architecture/0012-operator-tui.md) — the accepted decision
- [ADR 0011](../../architecture/0011-auth.md) — auth, and the absent scope concept
- [ADR 0013](../../architecture/0013-custom-raft.md) / `docs/phase-5-plan.md` — the HA reality this design accounts for
- [ADR 0014](../../architecture/0014-wasm-scheduling-policies.md) / `docs/journal/phase-6.md` — the policy counters this exposes
- `docs/operations/running-a-cluster.md` — the per-node registry requirement
- `2026-05-17-brokkr-frontend-design.md` — the web dashboard sharing this read-model
