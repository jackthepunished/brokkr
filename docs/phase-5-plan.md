# Phase 5 — Consensus & HA (custom Raft)

> **Status (2026-07-30):** milestones **I0–I9a shipped**. `origin/main` carries
> I0–I8c (I8c landed via PR #162); `origin/feat/raft-ha` carries **I9a only** and
> is **not yet merged**.
> Definition-of-done scoreboard: **1 proven** (real-process failover ~290 ms),
> **2 proven** (deterministic sim + turmoil), **3 pending** (the 1M-operation
> certification run). The remaining work is specified in
> [Part VII](#part-vii--remaining-work-i9bi10).
>
> **Layout:** Parts I–VI are the original pre-implementation design contract,
> preserved verbatim — the P-number pitfall codex in
> [Part V](#part-v--the-pitfall-codex-p-numbers-referenced-throughout) is cited
> by name throughout the journal, so it stays authoritative. Part VII is the
> plan for finishing the phase. Per-increment retrospectives live in
> [`docs/journal/phase-5.md`](journal/phase-5.md), not here.
>
> **Audience:** the engineer (human or AI) executing Phase 5. Read Part VII
> first if you are picking the work up mid-phase. When this document conflicts
> with `docs/plan.md`, the plan wins — flag the conflict to the owner instead of
> silently deviating.

---

## Part I — The mission

`docs/plan.md` §17, restated: **replace the control plane's embedded metadata
store with a from-scratch Raft implementation so the control plane survives
node loss.** This is the project's educational centerpiece.

### Definition of Done (verbatim from plan §17 — the finish line)

1. **Kill the leader → the cluster elects a new one in < 2 s.**
2. **Partition the cluster → the minority side stops accepting writes;
   rejoin → consistent.**
3. **Run 1,000,000 operations under fault injection with zero divergence.**

### The one absolute rule

**Hard rule 10: never bring in an existing Raft crate** — no `raft-rs`, no
`openraft`, no `little-raft`, not as a dependency, not vendored, not
copy-pasted. Implement from the paper (Ongaro & Ousterhout, *In Search of an
Understandable Consensus Algorithm*, the extended version) and the Raft thesis.
Reading etcd/TiKV *source for ideas* is encouraged (plan reading list Tier 4);
importing their code is not. If you are ever tempted, stop and ask the owner.

### What exists at Phase 5 start (verified against the tree)

- Phases 0–4 merged. The control plane (`brokkr-control`) is a single-node
  tonic gRPC server. Its durable state is two single-file redb databases
  opened from `--data-dir` ([main.rs](../crates/brokkr-control/src/main.rs)):
  - `cas.redb` — blob store, table `blobs: &str → &[u8]`
    ([redb_backend.rs](../crates/brokkr-cas/src/redb_backend.rs));
  - `action_cache.redb` — prost-encoded REAPI `ActionResult` keyed by action
    digest hash, table `action_results: &str → &[u8]`
    ([action_cache.rs](../crates/brokkr-cas/src/action_cache.rs)).
- All *scheduler* state (registry, fair queue, leases, quotas) is **in-memory
  and ephemeral by design** (ADRs 0008–0010): workers re-register on
  reconnect, leases reassign on crash. Do not assume it must replicate.
- Internal protos live in
  [`crates/brokkr-proto/protos/brokkr/v1/`](../crates/brokkr-proto/protos/brokkr/v1/)
  (`worker.proto`, `membership.proto`) — `raft.proto` goes beside them,
  compiled by the existing `tonic-build` setup in `build.rs`.
- ID newtypes live per-domain; follow the pattern in
  [`brokkr-common/src/ids.rs`](../crates/brokkr-common/src/ids.rs)
  (`WorkerId`, `JobId`, `TenantId`). Raft-domain newtypes live in
  `brokkr-raft`, not in common.
- Workspace deps already available: `tokio 1`, `tonic 0.12`, `prost 0.13`,
  `redb 2`, `bytes 1`, `thiserror 1`, `tracing 0.1`, `rand` (check root
  `Cargo.toml` before assuming anything else). MSRV **1.85**, edition 2021.
- `turmoil` (Tokio-native deterministic network simulation) is
  **pre-approved** by plan §7 (tech stack) and §21 (testing strategy). It is
  the only new dependency this phase is expected to need. Any other new dep →
  stop and ask, with a one-line rationale ready.

---

## Part II — Operating contract (how every increment ships)

### Hard rules (CLAUDE.md — violations are never acceptable)

1. No `unwrap()`/`expect()` in library crates — `?` + `thiserror` enums.
   Tests may use `unwrap()` (the workspace clippy allow-list covers `unwrap`
   in tests but **not** `expect` — use `unwrap`).
2. No `unsafe` without a `// SAFETY:` comment (Raft should need none).
3. Never disable a failing test; `#[ignore]` + reason + tracking issue only.
4. New deps require a one-line rationale in the PR description.
5. Lockfile changes are their own commit — never a side effect.
6. No Docker/runc/containerd anywhere. No existing Raft crate (rule 10).
7. Commits: conventional (`feat(raft): …`), **no Co-Authored-By trailer**.

### Workflow per increment

1. State the goal in one sentence; name the affected crates.
2. Tests in the same commit as the implementation — for protocol behavior,
   write the test *first* (they read as executable statements of the paper).
3. Every new RPC handler / hot-path async fn gets a `tracing` span
   (`tracing::info_span!("raft::append_entries")` style, fields not
   interpolation).
4. Rustdoc on every public item; the crate should build under
   `#![deny(missing_docs)]` like the rest of the workspace.
5. Update `CHANGELOG.md` (`## [Unreleased]`) and append a journal entry to
   this file (see [Journal](#journal) for the entry template).
6. One PR per milestone (I0–I10). PR description: motivation, what changed,
   how it was tested, plan §17 task reference. Never self-merge.

### Verification (WSL2 is the oracle; Windows checkout has CRLF noise)

```sh
# In WSL2, from /mnt/d/dev/brokkr (or a worktree path):
export CARGO_TARGET_DIR=$HOME/.brokkr-target
cargo fmt -p brokkr-raft -- --check
cargo clippy -p brokkr-raft --all-targets -- -D warnings
cargo test -p brokkr-raft
RUSTDOCFLAGS=-Dwarnings cargo doc -p brokkr-raft --no-deps
# When brokkr-control or brokkr-proto change, run the same for those crates.
```

Full-workspace `cargo test` is **not** green under WSL2 (pre-existing sandbox
seccomp + CRLF issues, unrelated to this phase). Per-crate green + Linux CI
(x86_64 + aarch64) is the bar. `cargo deny` runs in CI — when adding
`turmoil`, check its license (MIT) passes `deny.toml` and commit the
`Cargo.lock` change separately.

### Working-tree discipline

Do all work in **git worktrees off `origin/main`** (`git worktree add …`),
never on a shared checkout that may hold the owner's uncommitted work. The
operator TUI (ADR 0012) is the owner's workstream — never touch it.

### Stop-and-ask triggers (AskUserQuestion, with options + a recommendation)

- Any architectural fork not pinned by plan.md or ADR 0013 once ratified
  (storage schema changes, transport shape, snapshot format, read
  linearizability approach).
- What replicates through Raft in I8 (this one is *mandatory* — see I8).
- Deviating from joint consensus (plan §17 explicitly names it).
- Any new dependency beyond turmoil.
- A red test you cannot explain, or a safety property you cannot pin with a
  deterministic test.

---

## Part III — Architecture (ADR 0013 material)

Everything in this part is the *recommended* design. **I1 writes it up as ADR
0013 and gets owner sign-off before any implementation lands.** Present the
forks below as options with this document's recommendation first.

### III.1 The core is sans-IO (the load-bearing decision)

Implement the Raft state machine as a **pure, single-threaded, IO-free core**:

```rust
/// The entire Raft protocol, no IO, no clocks, no threads.
pub struct RaftCore {
    id: NodeId,
    role: Role,                    // Follower | Candidate | Leader
    hard: HardState,               // current_term, voted_for  (persisted)
    log: LogView,                  // in-memory index over Storage
    commit_index: LogIndex,
    last_applied: LogIndex,
    peers: BTreeMap<NodeId, Peer>, // next_index / match_index on the leader
    ticks: TickState,              // election + heartbeat tick counters
    config: RaftConfig,
}

impl RaftCore {
    /// Advance logical time by one tick. May fire election/heartbeat.
    pub fn tick(&mut self) -> Vec<Effect>;
    /// Feed one inbound message. Returns what must happen next.
    pub fn step(&mut self, from: NodeId, msg: Message) -> Vec<Effect>;
    /// Leader-only: append a client command to the log.
    pub fn propose(&mut self, data: Bytes) -> Result<LogIndex, RaftError>;
}

/// Everything the core wants the outside world to do. The shell owns
/// the ordering guarantee: `Persist` effects MUST complete (fsync) before
/// any `Send` produced by the same step() call is transmitted.
pub enum Effect {
    Persist(HardState),                  // term/vote changed — fsync first
    AppendLog(Vec<Entry>),               // stable-append before ack
    TruncateLog { from: LogIndex },      // conflict repair
    Send(NodeId, Message),
    Apply(Entry),                        // committed — feed the state machine
    SnapshotRequest,                     // core suggests compaction (I6)
}
```

Why this shape (put this argument in the ADR):

- **Determinism is the whole game.** The DoD demands 1M ops under fault
  injection with zero divergence. A sans-IO core makes the *protocol* testable
  with a plain seeded loop — no tokio, no sleeps, no flakes: a simulator owns
  a `Vec<RaftCore>`, a message bag, and a `StdRng`, and can drop/reorder/
  duplicate/partition arbitrarily. This is how etcd's `raft` package and TiKV
  structure it, for the same reason.
- **Time is ticks, not clocks.** The core never reads a clock. The production
  shell calls `tick()` from a `tokio::time::interval`; the simulator calls it
  from a loop. Election timeout = a *randomized* tick count re-rolled every
  reset (see pitfall P3).
- **The shell is thin and boring.** A `RaftNode` (tokio task) owns the core, a
  `Storage`, a `Transport`, and an mpsc of inbound messages + client
  proposals; its loop is: recv → `step`/`tick` → execute effects in order.
  All the hard bugs live in the core, where they are deterministic.

turmoil then tests the *networked shell* (gRPC transport + real
`RaftNode`s on a simulated network), while the pure simulator hammers the
core. Two layers of determinism; plan §17 task 3's turmoil requirement is
satisfied at the shell layer.

### III.2 Crate layout

```
crates/brokkr-raft/
├── src/
│   ├── lib.rs           #![deny(missing_docs)]; re-exports; crate docs
│   ├── types.rs         Term, LogIndex, NodeId, Entry, HardState, RaftConfig
│   ├── message.rs       Message enum: RequestVote{,Reply}, AppendEntries{,Reply},
│   │                    InstallSnapshot{,Reply} — pure Rust types, prost-independent
│   ├── error.rs         RaftError (thiserror): NotLeader{leader_hint}, Storage(..), …
│   ├── core/
│   │   ├── mod.rs       RaftCore: step/tick/propose dispatch, term rules (P2)
│   │   ├── election.rs  candidate logic, vote granting, election restriction
│   │   ├── replication.rs  leader: next/match index, commit rule (P4); follower:
│   │   │                consistency check, conflict truncation (P5)
│   │   └── snapshot.rs  (I6) compaction trigger + InstallSnapshot handling
│   ├── storage.rs       Storage trait + MemStorage (tests)
│   ├── storage_redb.rs  RedbStorage (production)
│   ├── transport.rs     Transport trait (async, object-safe)
│   ├── node.rs          RaftNode: the tokio shell (effect executor)
│   └── sim.rs           #[cfg(any(test, feature = "sim"))] deterministic simulator
│                        (exposed via feature so integration tests + I9 harness reuse it)
└── tests/
    ├── election.rs      I3 matrix
    ├── replication.rs   I4 matrix (incl. figure8 regression)
    ├── sim_faults.rs    I5: seeded fault campaigns + linearizability check
    ├── snapshot.rs      I6
    └── membership.rs    I7
```

Dependency direction: `brokkr-raft` → `brokkr-common` only (plus `prost`/
`bytes`/`thiserror`/`tracing`/`redb`/`rand`). The tonic/gRPC transport impl
lives in **brokkr-control** (which already depends on proto + tonic), keeping
brokkr-raft free of tonic and the crate DAG clean. `message.rs` types get
`From`/`TryFrom` conversions to the `raft.proto` prost types, colocated with
the transport impl in brokkr-control.

Files >500 lines are a smell — split before it happens; `core/` is deep on
purpose.

### III.3 Types (follow the ids.rs newtype pattern)

```rust
/// Monotonic election epoch. Ord is load-bearing (P2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Term(pub u64);

/// 1-based log position. 0 = "before the first entry" sentinel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct LogIndex(pub u64);

/// Stable node identity (survives restarts; assigned in config, not random).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId(String);

/// One log slot. `data` is opaque to Raft (it's the KV command in I8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub term: Term,
    pub index: LogIndex,
    pub payload: EntryPayload,   // Command(Bytes) | ConfChange(..) | Noop
}

/// The two fields that MUST hit disk before any related message leaves (P1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HardState {
    pub current_term: Term,
    pub voted_for: Option<NodeId>,  // NodeId is not Copy; adjust derives
}
```

`EntryPayload::Noop`: a new leader appends a no-op in its own term
immediately on election — the standard trick to force the commit rule (P4)
forward without waiting for client traffic. Include from I4.

### III.4 Storage

```rust
/// Persistence contract. Implementations MUST make each call durable
/// (fsync) before returning Ok — the core's correctness depends on it.
pub trait Storage: Send + 'static {
    fn save_hard_state(&mut self, hs: &HardState) -> Result<(), StorageError>;
    fn hard_state(&self) -> Result<HardState, StorageError>;
    fn append(&mut self, entries: &[Entry]) -> Result<(), StorageError>;
    /// Delete entries at ≥ from (conflict repair). Keeps earlier entries.
    fn truncate_from(&mut self, from: LogIndex) -> Result<(), StorageError>;
    fn entry(&self, idx: LogIndex) -> Result<Option<Entry>, StorageError>;
    fn entries(&self, lo: LogIndex, hi: LogIndex) -> Result<Vec<Entry>, StorageError>;
    fn last_index(&self) -> Result<LogIndex, StorageError>;
    /// (I6) Atomically: persist snapshot meta + drop the log prefix ≤ snap.index.
    fn install_snapshot_meta(&mut self, meta: &SnapshotMeta) -> Result<(), StorageError>;
    fn snapshot_meta(&self) -> Result<Option<SnapshotMeta>, StorageError>;
}
```

`RedbStorage` — a third redb database, `raft.redb` in `--data-dir`:

| Table | Key | Value | Notes |
|---|---|---|---|
| `raft_meta` | `&str` (`"hard_state"`, `"snapshot_meta"`) | `&[u8]` prost | matches the existing `&str → &[u8]` house style |
| `raft_log` | `u64` (index) | `&[u8]` prost-encoded `Entry` | u64 keys give ordered range scans + O(1) last_index via `last()` |

Encode with prost (define storage messages in `raft.proto` too) — the repo
already stores prost bytes in redb (`action_results`); don't introduce bincode
here. redb transactions are sync → the shell wraps storage calls in
`spawn_blocking` exactly as `RedbCas` does. `MemStorage` (BTreeMap) ships in
I1 for the core tests and the simulator.

**Crash-consistency test obligation (I2):** kill -9 the process between
"append entries" and "reply" must never lose an acked suffix; a torn write of
`hard_state` must never resurrect an older term/vote. redb's transactional
guarantees do the heavy lifting — the tests prove we *use* them correctly
(one commit per Effect batch, hard-state and log writes in the correct
order: hard state first).

### III.5 Transport

```rust
/// Fire-and-forget message delivery. Raft tolerates loss/reorder/duplication,
/// so the transport makes NO delivery guarantee — no retries in the transport
/// (the protocol retries via ticks). Object-safe for test doubles.
#[async_trait::async_trait]
pub trait Transport: Send + Sync + 'static {
    async fn send(&self, to: &NodeId, msg: Message);
}
```

Production impl (in brokkr-control): `raft.proto` service

```proto
service RaftTransport {
  // Single RPC; the Message enum rides in a oneof. Fire-and-forget replies
  // travel as their own Exchange calls, not RPC responses — this keeps
  // send() one-way and lets turmoil/partition tests reason per-message.
  rpc Exchange(RaftMessage) returns (ExchangeAck);
}
```

mTLS via the existing worker-mTLS flags (ADR 0011 posture: Raft peers are
infrastructure, same trust domain as workers). Peer addresses come from a
static `--raft-peer` flag list in I9 (id=addr pairs); dynamic discovery is
out of scope this phase.

### III.6 Timing model (constants live in `RaftConfig`, not scattered)

| Constant | Default | Rationale |
|---|---|---|
| tick interval (shell) | 10 ms | fine-grained enough for the <2 s DoD |
| heartbeat | 5 ticks (50 ms) | leader sends empty AppendEntries |
| election timeout | randomized **15–30 ticks** (150–300 ms), re-rolled on every reset | paper's range; re-roll prevents lockstep (P3) |
| max entries per AppendEntries | 256 | bounds message size; tune later |

Worst realistic re-election ≈ timeout + vote round ≈ well under 500 ms —
comfortably inside the 2 s DoD even with a couple of split votes.

### III.7 ADR 0013 checklist (write-up = milestone I1)

Decisions to record, each with alternatives + consequences: sans-IO core
(vs actor-per-role, vs async-in-core); tick-based time; crate boundary
(tonic stays in brokkr-control); redb schema above; prost-for-storage;
fire-and-forget transport; static peer config; joint consensus (pinned by
plan §17 — record, don't re-decide); ReadIndex deferral (see I8).

---

## Part IV — Milestones

One PR each. Do not start milestone N+1 while N has unresolved review
feedback (fixing CI on a merged milestone is fine). Estimated sizes are for
scoping honesty, not deadlines.

### I0 — Raft notes (`docs/raft-notes.md`) — docs PR

Plan §17 task 1. Not busywork: the notes are the reference the code reviews
cite. Required sections:

1. **State + RPC cheat sheet** — Figure 2 of the paper transcribed and
   annotated in your own words (every field, every rule, *why* it exists).
2. **The five safety properties** (Election Safety, Leader Append-Only, Log
   Matching, Leader Completeness, State Machine Safety) and the argument
   chain connecting them.
3. **The two subtle rules**, each with a worked scenario: the election
   restriction (§5.4.1 — voter denies candidates with stale logs) and the
   Figure-8 commit rule (§5.4.2 — never commit prior-term entries by
   counting; commit them transitively behind a current-term entry).
4. **Snapshotting** (§7) and **joint consensus** (§6 + thesis ch. 4) — enough
   detail that I6/I7 need no re-reading.
5. **A pitfalls appendix** — start from Part V below, extend it with anything
   the paper/thesis reading surfaces.

Exit: doc merged. No code.

### I1 — ADR 0013 + `brokkr-raft` scaffold

- Write ADR 0013 per §III.7. **Present the forks to the owner
  (AskUserQuestion) and get sign-off before the scaffold PR merges.**
- New crate wired into the workspace: `types.rs`, `message.rs`, `error.rs`,
  `storage.rs` + `MemStorage`, `Transport` trait, empty `RaftCore` with
  `step`/`tick`/`propose` signatures returning `Vec<Effect>`.
- `#![deny(missing_docs)]`, clippy config inherited, CI picks the crate up
  automatically (workspace member).

Tests: `MemStorage` contract tests (append/truncate/range/last_index
roundtrips) — written against `dyn Storage` so `RedbStorage` reuses the
suite in I2. Exit: crate green in WSL2 + CI; ADR accepted.

### I2 — Persistent state (`RedbStorage`)

Plan §17 task 2 (persistence slice). `storage_redb.rs` per §III.4.

Tests (same `dyn Storage` suite as MemStorage, plus):
- reopen-after-drop: hard state + log survive.
- torn-write ordering: hard state committed in its own redb txn *before*
  dependent sends could exist (test via Effect-order assertions in core
  tests + storage-level txn tests here).
- truncate_from(k) then append: no ghost entries; last_index correct.
- 10k-entry append/scan under `spawn_blocking` (smoke, not a benchmark).

Exit: both storages pass one shared suite. **No protocol logic yet.**

### I3 — Leader election

Plan §17 task 2 (RequestVote + roles). Implement in `core/election.rs` +
term rules in `core/mod.rs`:

- Follower: election-tick timeout → become candidate; reset timer **only**
  on (a) granting a vote, (b) AppendEntries/InstallSnapshot from the
  *current* leader (P3).
- Candidate: increment term, vote self, `Persist` then `Send` RequestVote to
  all; majority → leader; AppendEntries from a leader with term ≥ own →
  step down; timeout → new election (re-rolled timeout).
- Voter: at most one vote per term (persisted, P1); **election restriction**:
  deny unless candidate's (lastLogTerm, lastLogIndex) ≥ voter's (P6).
- Universal term rule (P2): any message with a higher term → adopt term,
  clear vote, become follower, *then* process; any with a lower term →
  reply with current term (reject) and otherwise ignore; stale *replies*
  (from an older term or a role we've left) are dropped (P7).
- Leader on election: initialize next/match, append a Noop in its own term.

Test matrix (`tests/election.rs`, all via MemStorage + direct step/tick —
no tokio):

| Test | Asserts |
|---|---|
| single node elects itself | 1-node cluster → leader after timeout |
| three nodes, no faults | exactly one leader; terms agree |
| split vote resolves | force simultaneous candidates (same tick) → someone wins within bounded ticks thanks to re-rolled timeouts |
| election restriction | stale-log candidate never wins against a majority with longer logs |
| one vote per term durable | vote, "crash" (rebuild core from storage), same-term second candidate is denied |
| higher term demotes leader | leader receiving term+1 message steps down |
| disruptive old candidate | rejoining node with high term but stale log cannot win but does force re-election (documents the known Raft wart; pre-vote noted as future work, not implemented) |

Exit: matrix green; every pitfall P1–P3, P6, P7 has a named regression test.

### I4 — Log replication

Plan §17 task 2 (AppendEntries + safety). `core/replication.rs`:

- Leader: per-peer nextIndex/matchIndex; on AppendEntries reject, decrement
  nextIndex and retry (linear backup is fine; note the fast-backup
  optimization as future work); advance commitIndex = the largest N where a
  **majority** matchIndex ≥ N **and log[N].term == currentTerm** (P4).
- Follower: consistency check on (prevLogIndex, prevLogTerm); on conflict
  truncate from the first conflicting entry **only** (P5 — never blind-
  truncate on a duplicate/reordered older AppendEntries); append; advance
  commitIndex to min(leaderCommit, last new entry).
- Apply loop: `Effect::Apply` in strict index order, exactly once (P8).
- `propose()` on non-leader → `RaftError::NotLeader { leader_hint }`.

Test matrix (`tests/replication.rs`):

| Test | Asserts |
|---|---|
| happy-path replication | 3 nodes, N proposals → identical logs, all applied once, in order |
| follower catch-up | disconnected follower rejoins → nextIndex backup repairs it |
| conflict truncation | follower with divergent uncommitted suffix gets it replaced |
| duplicate/reordered AE idempotent | replaying old AppendEntries never truncates or double-applies (P5, P8) |
| **figure_8 regression** | reproduce the paper's Figure 8 step by step; assert the old-term entry is NOT committed by counting, and IS committed transitively once a current-term entry commits (P4) |
| leader noop commits | fresh leader with no client traffic still advances commit via its Noop |
| commit only with majority | 2/5 matchIndex does not commit |

Exit: matrix green. The core is now "Raft minus snapshots/membership".

### I5 — Deterministic simulation + turmoil (plan §17 task 3)

Two layers:

**(a) Pure simulator (`sim.rs`)** driving `RaftCore`s directly:

```rust
pub struct Sim {
    nodes: BTreeMap<NodeId, SimNode>,     // core + MemStorage + inbox
    net: MessageBag,                       // pending (from, to, msg, deliver_at)
    rng: StdRng,                           // SEEDED — print seed on failure
    oracle: Oracle,                        // committed-entry history checker
}
```

Fault campaign per sim step (probabilities from the seed): drop / duplicate /
delay-reorder messages; partition into arbitrary groups; heal; crash a node
(drop core, keep storage) and restart it (rebuild core *from storage* —
this is what makes crash-mid-write real); client proposals to random nodes.

**The oracle (linearizability for this workload):** every committed entry is
reported (node, index, term, payload). Assert: (1) **prefix agreement** — no
two nodes ever report different (term, payload) at the same index; (2)
**commit monotonicity** — no committed index is ever un-committed by a later
report from the same node post-restart; (3) **exactly-once apply** per node
per index. For a log state machine, prefix agreement across all nodes at all
times *is* linearizability of committed writes; write this argument in the
test-module docs. (KV-level read linearizability is I8's concern.)

CI shape: `sim_faults.rs` runs a fixed set of seeds (say 32) × 10k steps in
normal CI, and one `#[ignore]`d long campaign (`RAFT_SIM_STEPS=1M`,
release mode) that I9's DoD run invokes explicitly. Any failure prints the
seed; add every failing seed found during development to the fixed set as a
permanent regression.

**(b) turmoil suite** for the shell: `RaftNode` + gRPC-shaped transport over
`turmoil::net` (the transport trait makes this a small adapter; if tonic
over turmoil proves gnarly, a length-prefixed prost codec over
`turmoil::net::TcpStream` is an acceptable stand-in for the *simulated*
transport — the wire format is not what's under test; note the choice in the
PR). Scenarios: partition leader from majority → old leader stops
committing, new leader elected, heal → old leader's uncommitted suffix
repaired; restart under load; hold `Persist` latency high to widen crash
windows.

Adding turmoil: dev-dependency, workspace-level, own lockfile commit,
license check via `cargo deny`. Exit: both layers green in CI; seeds
reproducible.

### I6 — Snapshots / log compaction (plan §17 task 4)

- `SnapshotMeta { last_included_index, last_included_term, conf }` + the
  snapshot blob itself (opaque `Bytes` from the state machine's
  `snapshot()` callback — for I8's KV this is a serialized map).
- Trigger: log length > `RaftConfig::snapshot_threshold` (default 8192
  entries) → `Effect::SnapshotRequest`; shell asks the state machine for a
  snapshot, then `install_snapshot_meta` (atomic with prefix drop, §III.4).
- `InstallSnapshot` RPC (single-shot, not chunked — entries are small KV
  commands; note chunking as future work) for followers whose nextIndex has
  been compacted away.
- Restart path: state machine restores from snapshot, then replays the log
  tail (P9: applied index must never regress below snapshot index).

Tests: compaction preserves semantics (sim with tiny threshold — snapshots
exercised *constantly* under the I5 fault campaign); lagging follower
catch-up via InstallSnapshot; restart-from-snapshot + tail replay; stale
InstallSnapshot (older than current snapshot) ignored. Exit: I5 campaign
green with `snapshot_threshold = 16`.

### I7 — Membership changes (joint consensus, plan §17 task 5)

- `EntryPayload::ConfChange(ConfChange)` with joint consensus exactly per
  paper §6: C_old,new entry → **agreement requires majorities in BOTH
  configs**; once C_old,new commits, leader appends C_new; once C_new
  commits, old-only members shut down.
- Configuration is applied **when the entry is appended, not committed**
  (paper rule) — and rolled back if truncated (P10; this is the nastiest
  edge, test it explicitly).
- New nodes join as learners (non-voting, replicated to but not counted)
  until caught up, then the joint change runs — thesis ch. 4 catch-up
  safeguard; without it, adding a fresh node can stall the cluster.
- One in-flight conf change at a time (reject `propose_conf_change` while
  one is pending).

Tests: add node under load (sim); remove the *leader* (it steps down after
committing C_new that excludes it); no dual-majority window (oracle checks
prefix agreement across the transition); truncated C_old,new rolls the
config back; learner catch-up gate. Exit: I5 campaign green with random
membership churn added to the fault mix.

### I8 — Raft-backed KV in the control plane (plan §17 task 6)

**MANDATORY STOP-AND-ASK before writing code.** Propose to the owner what
replicates. Recommended framing (from the tree as it stands):

- **Replicate:** action-cache writes (the durable metadata with correctness
  value), and cluster-level config as it emerges.
- **Do NOT replicate:** CAS blob bytes (content-addressed; Phase 3 quorum
  replication already owns blob durability), scheduler queue / registry /
  leases (ephemeral by ADR design — workers re-register and leases reassign
  on failover).
- The precise cut changes the I9 story (what survives leader kill), so the
  owner decides.

Implementation shape:
- `MetaKv` trait in brokkr-control (get/put/delete/scan over `Bytes`),
  with (a) the existing single-node redb impl and (b) `RaftKv`: proposes
  prost-encoded commands, applies committed entries to a redb-backed
  materialized state machine, snapshot = serialized KV state.
- Writes on a follower → `NotLeader{leader_hint}` → gRPC `FAILED_PRECONDITION`
  with the leader's address in metadata; the SDK/client retries against the
  hint (plan task 7: "clients can talk to any; followers redirect").
- **Reads:** default = leader-local reads after applying up to the read
  index… full ReadIndex is the correct linearizable-read mechanism; propose
  it as an ask-point: (recommended) implement ReadIndex (small once
  heartbeats exist) vs. document reads-may-be-stale-on-followers for this
  phase.

Tests: `MetaKv` contract suite runs against both impls; 3-node in-process
cluster: write via leader, kill leader, read after failover; follower write
redirect; action-cache integration behind the trait. Exit: brokkr-control
green with either impl selected by config (`--raft` off ⇒ exactly today's
behavior; single-node default unchanged).

### I9 — HA control plane + Jepsen-style DoD run (plan §17 tasks 7–8)

- `brokkr-control` flags: `--node-id`, `--raft-peer id=addr` (repeated),
  `--raft` on/off. Three-process bring-up documented in
  `docs/operations/running-a-cluster.md` (extend, don't fork, the existing
  doc); extend `scripts/run-cluster.sh` with an HA mode.
- Real-process integration test in the style of
  [two_process_cluster.rs](../crates/brokkr-control/tests/two_process_cluster.rs)
  (`#[ignore]`, spawns binaries): 3 control nodes + worker + `brokk run`;
  kill the leader; assert a subsequent `brokk run` succeeds and **measure
  time-to-new-leader < 2 s** (DoD 1).
- Partition semantics (DoD 2) proven in the turmoil layer (real-process
  partitioning needs root/netns — do not attempt; state this in the PR).
- **The 1M-op certification run (DoD 3):** the I5 `#[ignore]`d campaign,
  release mode, full fault mix + membership churn + tiny snapshot
  threshold, ≥1,000,000 client operations, oracle green, seed(s) recorded
  *in this journal*. Runtime target: minutes, not hours (it's a pure-core
  loop).

Exit: all three DoD lines demonstrably true, with commands + seeds + timings
recorded in the journal entry.

### I10 — Phase 5 wrap-up

- §11 exit-criteria review (rustdoc, integration test per capability,
  tracing coverage, unit coverage on logic-heavy modules) — honest
  done-vs-deferred, same format as the Phase 4 wrap-up in `phase-4.md`.
- Retrospective: what the paper hid, what the simulator caught, what
  turmoil caught that the pure sim didn't (and vice versa).
- Deferred list (expected: pre-vote, leadership transfer, fast log backup,
  chunked InstallSnapshot, ReadIndex-if-deferred, dynamic peer discovery,
  Raft-metrics dashboard).
- CHANGELOG + README roadmap row (Phase 5 → done) + plan §11 table if the
  owner agrees. Then **stop — Phase 6+ is a new owner conversation.**

---

## Part V — The pitfall codex (P-numbers referenced throughout)

Every one of these is a real, commonly-shipped Raft bug. Each must have a
named regression test by the milestone indicated.

| # | Pitfall | The rule | Test lives in |
|---|---|---|---|
| P1 | Replying before persisting | `Persist`/`AppendLog` effects complete (fsync) before any `Send` from the same step. Vote + term + acked entries are promises; a promise that doesn't survive a crash is a safety hole. | I2/I3 |
| P2 | Sloppy term handling | Higher term in ANY message (including replies): adopt, clear vote, → follower, then process. Lower term: reject/ignore. One code path in `core/mod.rs`, not per-handler copies. | I3 |
| P3 | Election timer reset abuse | Reset ONLY on: granting a vote; AppendEntries/InstallSnapshot from the current-term leader. NOT on RequestVote received, NOT on rejected AEs. Re-roll the randomized timeout on every reset or nodes lockstep into split votes forever. | I3 |
| P4 | Committing prior-term entries by counting | commitIndex advances only to N with `log[N].term == currentTerm` (+ majority match). Prior-term entries commit transitively. This is Figure 8; getting it wrong loses committed writes. | I4 (`figure_8`) |
| P5 | Blind log truncation | Truncate only from the first *conflicting* entry. A duplicate/reordered old AppendEntries whose entries all match must be a no-op, or reordered RPCs erase acked entries. | I4 |
| P6 | Skipping the election restriction | Voter denies candidates whose (lastLogTerm, lastLogIndex) is behind its own. Without it, a stale leader erases committed entries. | I3 |
| P7 | Acting on stale replies | A reply carrying an old term, or arriving after role/term changed, is dropped. Tag in-flight expectations with the term they were sent in. | I3/I4 |
| P8 | Apply-loop races | Entries applied in index order, exactly once, only at commit. Keep `last_applied` core-internal; emit `Effect::Apply` — the effect queue is naturally ordered. | I4 |
| P9 | Snapshot/log interleaving | Never drop log prefix before snapshot meta is durable (one atomic storage op). Applied index never regresses below snapshot index on restart. | I6 |
| P10 | Conf-change edge | Config applies at *append* (not commit) and must roll back on truncation. One in-flight change at a time. Learners before voting membership. | I7 |
| P11 | Randomness/time in the core | The core takes an RNG seed via config and counts ticks. Any `Instant::now()`/`thread_rng()` inside `core/` is a bug — it kills sim determinism. Enforce by review + a clippy `disallowed_methods` entry for the crate if practical. | I1 onward |

---

## Part VI — Test strategy summary (what CI runs at phase end)

1. **Storage contract suite** — shared across MemStorage/RedbStorage (I1/I2).
2. **Protocol matrices** — election + replication + snapshot + membership
   tables above; pure core, no tokio, milliseconds to run (I3–I7).
3. **Pitfall regressions** — P1–P11, named for greppability (`test figure_8`,
   `test p5_duplicate_append_is_noop`, …).
4. **Seeded sim campaigns** — 32 fixed seeds × 10k steps in CI; failing seeds
   become permanent fixtures (I5+, extended by I6/I7).
5. **turmoil shell scenarios** — partition/heal, restart-under-load (I5).
6. **Real-process HA test** — `#[ignore]`, leader-kill + <2 s measurement (I9).
7. **The 1M-op certification** — `#[ignore]`, release mode, run for the DoD
   and on demand before each later Raft-touching change (I9).

---
## Part VII — Remaining work (I9b–I10)

Written 2026-07-30, against `origin/main` @ `5f554fb` and
`origin/feat/raft-ha` @ `bba53cf`. Everything below is verified against those
two trees, not against the local checkout.

`origin/feat/raft-ha` is **4 commits behind `main`** (the issue #144 CAS GC
barrier fix and the #162 / #158 merges), so merge `main` into it — or rebase —
before building on it. That gap is the whole reason this section pins commit
hashes: I8c moved from "unmerged branch" to `main` in the hours between the
first read of the tree and this sentence.

### VII.0 Where the phase actually stands

| Milestone | Content | Where it lives |
|---|---|---|
| I0 | `docs/raft-notes.md` | `origin/main` |
| I1 | ADR 0013 + `brokkr-raft` scaffold | `origin/main` |
| I2 | crash-safe hard state on redb | `origin/main` |
| I3 | leader election | `origin/main` |
| I4 | log replication + Figure-8 rule | `origin/main` |
| I5a–c | deterministic simulator, `RaftDriver`, tonic-over-turmoil | `origin/main` |
| I6 | snapshots + `InstallSnapshot` | `origin/main` |
| I7a–c | entry payloads, joint consensus, learners | `origin/main` |
| I8a–b | `MetaKv` seam, apply loop + ReadIndex | `origin/main` |
| I8c | `RaftKv` + `NotLeader` redirect + `--raft` | `origin/main` (PR #162) |
| **I9a** | HA control plane, `--node-id`/`--raft-peer`/`--raft-listen`, DoD 1 | **`feat/raft-ha`, unmerged** |
| I9b | leader-aware clients and workers | **this plan** |
| I9c | the 1M-operation certification (DoD 3) | **this plan** |
| I9d | raft-plane mTLS | **this plan** |
| I10 | wrap-up + §11 exit-criteria review | **this plan** |

**DoD scoreboard** (plan §17):

1. **< 2 s failover — PROVEN.** 291.9 ms / 288.2 ms on three real
   `brokkr-control` processes (`tests/raft_ha_cluster.rs`, `#[ignore]`, WSL2,
   debug build). 7× margin.
2. **Partition semantics — PROVEN** in
   `simulation.rs::minority_partition_cannot_commit_and_heals_consistently`
   and, over real gRPC, in
   `turmoil_cluster.rs::grpc_cluster_survives_leader_partition_and_heals`.
   Real-process partitioning needs root + netns and is deliberately not
   attempted (Part II).
3. **1M operations under fault injection — PENDING.** This is I9c and it is
   the only DoD line still open.

### VII.1 The four gaps between I9a and "HA works"

Read against `feat/raft-ha`. These are the concrete defects, not a wish list.

1. **The leader hint is not actionable.** `services/action_cache.rs` returns
   `FAILED_PRECONDITION` with `x-brokkr-leader: <node-id>`. A node id is not
   an address; nothing on the client side can turn `control-1` into a dial
   target. The I8c journal entry flagged this for I9 ("I9's peer wiring maps
   ids to addresses at the service edge") and I9a did not do it —
   `--raft-peer id=host:port` carries *raft-plane* addresses, which are not
   the client-plane listener.
2. **No client follows the redirect.** `brokkr-sdk::client` has no
   `FAILED_PRECONDITION` handling at all, and `brokk` takes a single
   `--control`. The I9a DoD test discovers the leader by trying all three
   nodes by hand, in the test body — the client library cannot do that.
3. **Workers pin one endpoint.** `WorkerConfig::control_endpoint` is a single
   `String` (`worker.rs`); `build_channel` dials once at startup. Kill that
   control node and the worker is gone until it is restarted by hand. The
   I9a entry lists this as known-missing.
4. **A follower fails an Execute *after* running the action.**
   `Scheduler::execute` writes the action cache unconditionally
   (`scheduler.rs`, `update_action_result` → `anyhow!("action cache update: …")`).
   Under `--raft` on a follower, that write returns
   `MetaKvError::NotLeader`, so the RPC fails `INTERNAL` having already
   burned a sandbox run. This is decision D1 below.

### VII.2 Decision D1 — RESOLVED: best-effort internal cache write

**Owner decision, 2026-07-30** (stop-and-ask per Part II — this sets the
control plane's active/passive posture, so it was the owner's call):

> **The scheduler's post-execution action-cache write is best-effort.** On
> `NotLeader` the build succeeds with its real result and the entry is simply
> not cached. A follower never refuses work and never discards a completed
> action.

Consequences, to be implemented rather than discovered:

- **Every node serves builds** — active/active for *execution*, single-writer
  for *replicated metadata*. A build routed to a follower runs correctly and
  populates CAS (CAS is not Raft-replicated, so blob traffic works on any
  node); only its action-cache entry is lost, so the next identical build
  re-executes instead of hitting the cache.
- **The degradation must be loud, not silent.** This was the recorded
  objection to the option, so the mitigation is part of the work: a `warn`
  span event with `action_digest`, `node_id`, and the leader hint as fields; a
  monotonic counter of uncached-because-not-leader results; and the fact
  surfaced in the `ExecuteResponse` execution metadata so a client can tell
  "not cached" from "cached". Best-effort must never mean unobservable.
- **External REAPI writes still redirect.** `ActionCacheService::update_action_result`
  is a public REAPI surface that Bazel calls directly; a client that asked for
  a write and got `OK` must actually have a write. That path keeps the
  `FAILED_PRECONDITION` + leader-hint redirect (W2/W3) — best-effort applies
  only to the *internal* write the scheduler issues on the client's behalf.
- **Rejected alternatives**, both recorded for the I10 deferred list:
  (a) refuse at admission with a leader hint (active/passive, one writer);
  (b) follower → leader forwarding via a client-proposal RPC on
  `RaftService` (active/active *including* metadata, etcd-style).

### VII.3 I9b — leader-aware clients and workers (§17 task 7)

**Goal:** a client or worker pointed at *any* control node keeps working
across a leader kill, with no manual intervention and no action executed
twice for want of a routing decision.

- **W1 — publish the client-plane topology through Raft.** Use the `cfg/`
  namespace that the I8 stop-and-ask explicitly reserved for "cluster-level
  configuration" — this is what it was reserved for. New
  `--advertise-addr` on `brokkr-control` (default: the client `--listen`
  value, with a hard error if that is a wildcard bind and no advertise
  address is given). Each node proposes `cfg/nodes/<node-id>` = its
  advertise address once it is serving; the value is a small prost or JSON
  record so a future field (raft addr, version, zone) does not need a new
  key. `RaftKv` resolves leader id → advertise address from its own applied
  `KvMachine` — no extra RPC, and a follower can answer because the config
  is replicated.
  *Tests:* `RaftKv` unit test that a proposed node record round-trips; a
  3-node test asserting every node resolves the same leader address.
- **W2 — make the redirect actionable.** `CasError::NotLeader` /
  `MetaKvError::NotLeader` carry `leader: Option<NodeId>` **plus**
  `leader_addr: Option<String>`. `cas_status` emits both
  `x-brokkr-leader` (unchanged, so the I8c test keeps its meaning) and a new
  `x-brokkr-leader-addr`. Keep the exhaustive `From<MetaKvError>` match — it
  is the forcing function that made I8c correct.
  *Tests:* extend
  `not_leader_maps_to_failed_precondition_with_a_leader_hint`; assert the
  unknown-leader case still emits neither key.
- **W3 — the SDK follows it.** `BrokkrClient::connect_any(endpoints)` plus
  redirect-following on `FAILED_PRECONDITION` + `x-brokkr-leader-addr`:
  re-dial the hinted address and retry once, **bounded at 3 hops**, then a
  typed `BrokkrError::NoLeader { attempted }`. Cache the last known leader so
  the steady state is one hop, not three. `brokk` gains a repeatable
  `--control` (single-value invocations keep working byte-for-byte).
  *Tests:* unit tests over a fake that redirects once, redirects in a cycle
  (must terminate), and hints an unreachable address (must fall back to the
  next configured endpoint).
- **W4 — worker endpoint rotation.** `WorkerConfig.control_endpoint` becomes
  an ordered `Vec<String>` (repeatable `--control`; the existing single-value
  form is the one-element case, and `--worker-control` split-port behavior is
  preserved per endpoint). On dial failure or a broken `PollJobs`/heartbeat
  stream, rotate to the next endpoint with jittered exponential backoff and
  **re-register** — registration is already idempotent per `WorkerId`, which
  W4 must assert rather than assume. Log the rotation at `warn` with both
  endpoints as fields.
  *Tests:* unit test for the rotation policy (pure function over
  attempt/endpoint list); integration test that a worker whose first
  endpoint refuses connections registers with the second.
- **W5 — implement D1 (best-effort internal write).** In `Scheduler::execute`,
  the post-execution `update_action_result` no longer fails the RPC on
  `MetaKvError::NotLeader`: the result is returned with a `warn` event
  (`action_digest`, `node_id`, leader hint), an
  `uncached_results_not_leader` counter increment, and a "not cached" marker
  in the response's execution metadata. **Only `NotLeader` degrades** — a
  genuine storage error still fails the RPC as it does today, so the
  best-effort path cannot swallow real corruption. One retry against the
  freshly-hinted leader before giving up covers the leadership-change race
  cheaply. `MetaKv` grows `leader_hint()` so the log line and the counter can
  name the leader. The external `ActionCacheService::update_action_result`
  path is untouched by this and keeps the W2 redirect.
  *Tests:* follower `Execute` returns the real result with `cache_hit = false`
  and the not-cached marker, and the job is *not* left in the queue; the
  counter increments; a non-`NotLeader` `MetaKvError` still fails the RPC
  (the regression that keeps best-effort from becoming best-ignored).
- **W6 — the end-to-end DoD test.**
  `crates/brokkr-control/tests/raft_ha_e2e.rs`, `#[ignore]`, spawning real
  binaries in the style of `two_process_cluster.rs` and
  `raft_ha_cluster.rs`: three control nodes + one worker + `brokk run`.
  1. `brokk run --command "echo hello"` against a **follower** → succeeds
     with the correct output, and reports not-cached (the D1 path, proven end
     to end rather than only in unit tests).
  2. The same action against the **leader** → succeeds and *is* cached. This
     is the write whose survival the test is about, so it must go through the
     leader; under D1 a follower-routed build populates nothing.
  3. SIGKILL the leader.
  4. A `brokk run` of a *new* action succeeds within a wall-clock budget
     (assert < 5 s including re-election; record the measured number in the
     journal).
  5. A `brokk run` of the step-2 action is a **cache hit** served by the new
     leader — proving the pre-kill metadata write survived, which is the whole
     point of replicating the action cache.
  6. Assert the worker rotated (the registry on the new leader lists it).

**Exit:** all six items landed, `--raft` off still byte-for-byte the
single-node behavior, and an I9b journal entry with the measured failover
numbers.

### VII.4 I9c — the 1,000,000-operation certification (DoD 3, §17 task 8)

The harness already exists: `simulation.rs` has the fault campaign
(`soak_random_faults`), constant compaction at `snapshot_threshold = 16`,
membership churn (`soak_random_faults_with_membership_churn`), and the
divergence oracle (`assert_no_divergence`). I9c turns it into a certification
run. Do **not** write a new simulator.

- **Count operations, not steps.** The soak loops over sim steps; the DoD
  counts *client operations*. Add an accepted-proposal counter to the harness
  and drive to ≥ 1,000,000, with `log`/`eprintln` progress every 50k.
- **Size it before running it.** Measure ops/sec with a 10k dry run in
  `--release` and compute the projected wall clock. The plan's target is
  minutes, not hours; if the projection is hours, the honest fix is a wider
  cluster-per-second rate (more proposals per step, batched), not a smaller
  op count.
- **The full fault mix, all at once:** latency jitter, reorder, partition and
  heal, crash and restart, membership churn (learner promote / voter
  removal), and `snapshot_threshold = 16` so compaction and
  `InstallSnapshot` run continuously. All three previously separate soaks
  combined — that combination is the part that has never been exercised.
- **Bound memory.** 1M entries × the sim's node count will not fit if
  compaction stalls; assert the log length stays bounded (a few × the
  threshold) as an invariant *inside* the loop, not just at the end. A
  growing log is itself a bug.
- **Oracle every N ops, not only at the end.** `assert_no_divergence` plus
  the committed-history prefix check every ~10k ops, so a failure names the
  operation that broke it instead of "somewhere in a million".
- **Record everything in the journal:** seeds used, ops/sec, wall clock,
  peak RSS, machine, build profile. Any seed that ever fails becomes a
  permanent `#[test]` fixture (Part VI rule 4) — that is non-negotiable.
- **Verify the CI campaign matches Part VI rule 4** (32 fixed seeds × 10k
  steps). If the current tests are a smaller set, widen them here; the
  certification run is `#[ignore]`, so CI needs the seeded campaign to have
  real coverage.

**Exit:** DoD line 3 flips to PROVEN in the journal scoreboard, with the
command anyone can re-run pasted in.

### VII.5 I9d — raft-plane mTLS (in scope; owner decision 2026-07-30)

Peer links are plaintext by I9a's explicit choice, and the owner chose to
close that inside Phase 5 rather than defer it: an unauthenticated RPC that
can append to the replicated log is not something to ship behind a journal
footnote.

- `--raft-tls-cert` / `--raft-tls-key` / `--raft-tls-ca` on
  `brokkr-control`, reusing the worker-plane mTLS plumbing already in
  `main.rs` rather than a second TLS code path. Peer *client* channels
  (`TonicTransport`'s endpoints) and the `--raft-listen` *server* both need
  it — a one-sided configuration is the failure mode to test for.
- **Refuse to start on a half-configured raft plane**, matching the posture
  issue #139 established for the client/worker planes: cert without key,
  TLS on the listener but not the dialers, or a CA that does not verify the
  configured cert are startup errors, not runtime surprises.
- **ADR 0011 amendment**, one section: three planes (client JWT, worker mTLS,
  raft mTLS), what each authenticates, and why the raft plane is
  mutual-only with no JWT.
- `docs/operations/running-a-cluster.md` gains the mTLS bring-up beside the
  existing plaintext one; `scripts/run-cluster.sh --ha` keeps working
  plaintext for local dev (documented as dev-only).
- *Tests:* a 3-node in-process cluster over mTLS that elects and replicates;
  a peer presenting no client cert is rejected; each half-configuration
  fails at startup with a message naming the missing flag.

### VII.6 I10 — Phase 5 wrap-up

- **§11 exit-criteria review** in the `phase-4.md` wrap-up format: rustdoc on
  every public API, ≥ 1 integration test per new capability, tracing spans on
  the new RPC handlers (`RaftService` handlers and the apply loop especially),
  unit coverage on the logic-heavy modules (`node.rs`, `driver.rs`,
  `raftkv.rs`). Honest done-vs-deferred, no rounding up.
- **Retrospective:** what the paper hid; what the deterministic sim caught
  that turmoil could not and vice versa; the no-op index churn (I8b) as a
  case study in tests over-fitting to absolute indices; the ReadIndex `seq`
  proof; the I8a exhaustive-`From` forcing function actually firing in I8c.
- **Deferred list** (expected): pre-vote, leadership transfer, fast log
  backup, chunked `InstallSnapshot`, follower reads at the read index,
  dynamic peer discovery / auto-join, Raft metrics dashboard, the two D1
  alternatives (admission-time refusal; follower → leader forwarding — either
  would make follower-routed builds cacheable), and durable scheduler state
  (job history / leases through Raft).
- **State the D1 posture in the phase summary,** not only in this plan: under
  `--raft`, a build routed to a follower is correct but uncached. That is a
  deliberate trade, and the uncached-result counter from W5 is how an operator
  notices they are paying for it.
- **Docs:** CHANGELOG, README roadmap row Phase 5 → done, `docs/plan.md` §11
  table (owner's call), and refresh
  `docs/brokkr-development-chronicle.md` — Phase 5 section, ADR table (0013
  is missing from it), and the counts.
- **Then stop.** Phase 6+ is a new owner conversation. The nearest candidates
  are already on record and should *not* be started inside Phase 5: the
  Bazel-compatibility DoD still open from Phase 4 (§16 task 9), and the
  operator TUI pulled forward under ADR 0012 (WS0–WS3, unstarted).

### VII.7 Sequencing

One PR per row, in order, each with its own journal entry (Part II).

| # | Branch | Content | Gate |
|---|---|---|---|
| P0 | *(merge)* `feat/raft-ha` → `main` | I9a (I8c already landed via #162) | `main` merged into the branch first, then CI green |
| P1 | `feat/raft-cluster-config` | W1, W2 | contract suite green on both `MetaKv` impls; hint tests |
| P2 | `feat/raft-client-failover` | W3, W4, W5 (D1) | unit + `raft_kv_cluster.rs`; `--raft` off unchanged |
| P3 | `test/raft-ha-e2e` | W6 | the `#[ignore]` run executed, numbers in the journal |
| P4 | `test/raft-1m-certification` | I9c | seeds + timings in the journal; DoD 3 proven |
| P5 | `feat/raft-plane-mtls` | I9d | mTLS cluster test green; half-configurations refuse to start |
| P6 | `docs/phase-5-wrapup` | I10 | §11 review, retrospective, README/CHANGELOG/chronicle |

**P0 is a prerequisite, not a formality.** A shipped milestone sitting on an
unmerged branch is how a stale-branch mistake happens — the project already
carries one (PR #96) as a standing reminder, and I8c drifting from "unmerged"
to `main` mid-plan is the same hazard in miniature. Re-read `origin/*` at the
start of every increment.

### VII.8 Risks

- **The 1M run's wall clock** is the schedule risk. Measure first (VII.4),
  and if the projection is bad, fix the proposal rate rather than quietly
  lowering the op count — the number is in the plan's DoD verbatim.
- **Real-process partition testing stays out of reach** (root + netns).
  DoD 2 is certified in simulation by design; say so plainly in the I10
  write-up rather than implying real-process coverage.
- **W4 touches the worker's connection lifecycle**, which is Phase 1 code
  every later phase depends on. Rotation must be a pure, unit-tested policy
  function with the I/O around it, not `if let Err(_) = … { try_next() }`
  sprinkled through `run_worker`.
- **D1's best-effort write can hide a routing problem as a cache-hit-rate
  problem.** A cluster whose clients all land on followers still *works*, so
  nothing fails and nobody looks — the builds just stop being cached. This is
  the recorded objection to the chosen option and the reason W5 ships the
  counter, the `warn` event, and the not-cached marker in execution metadata
  rather than only a log line. `docs/operations/running-a-cluster.md` must
  state the trade explicitly, and the I10 write-up must not describe the
  action cache as "replicated" without the follower caveat.
- **I9d touches TLS wiring shared with the worker plane.** Reuse, don't fork,
  the existing plumbing; a second TLS code path in `main.rs` is how the
  half-configured states that issue #139 closed get reintroduced.

### VII.9 Working-tree hygiene (do this before P0)

The local checkout this plan was written from is **163 commits behind
`origin/main`**, and the untracked `docs/journal/phase-5.md` (the original
pre-implementation plan, superseded by this file) **collides with the tracked
journal of the same path upstream** — a fast-forward will refuse until it is
removed. Delete it once this file is committed, then sync. The other
untracked docs (`docs/architecture/0012-operator-tui.md`, marked *accepted*,
and `docs/brokkr-development-chronicle.md`) are finished artifacts and should
be committed as their own docs PR rather than left to rot in the working
tree.
