# Phase 5 — Consensus & HA

- **Status:** in progress
- **Plan:** `docs/plan.md` §17
- **Started:** 2026-07-02

Goal: replace the single-node embedded metadata store with a **from-scratch
Raft** implementation (CLAUDE.md rule 10 — no external Raft crate), then stand
up a highly-available control plane on top of it. This is the educational
centerpiece of the project. This journal accumulates a short retrospective per
increment.

**Definition of done** (`docs/plan.md` §17): kill the leader → new leader in
< 2 s; partition the cluster → minority stops accepting writes, rejoin →
consistent; 1M operations under fault injection with zero divergence.

**Milestone map** (one PR each, in order): I0 paper notes · I1 ADR 0013 +
`brokkr-raft` scaffold · I2 persistent state on redb · I3 leader election ·
I4 log replication + Figure-8 test · I5 turmoil simulation suite · I6 snapshots
+ InstallSnapshot · I7 joint-consensus membership · I8 Raft-backed KV in
`brokkr-control` + leader redirect · I9 3-node HA + Jepsen-style harness ·
I10 wrap-up + §11 exit-criteria review.

## I0 — Raft paper notes (§17 task 1)

- **Date:** 2026-07-02
- **Affected:** docs only (`docs/raft-notes.md`).
- **Outcome:** `docs/raft-notes.md` — the implementation reference for
  `brokkr-raft`, taken from the extended Raft paper (Ongaro & Ousterhout, 2014)
  and the relevant thesis chapters. Covers the three server states and terms as
  a logical clock; the persistent (`currentTerm`, `votedFor`, `log`) vs. volatile
  state split and the **persist-before-respond** rule; leader election with
  randomized 150–300 ms timeouts; the RequestVote and AppendEntries RPCs step by
  step; the `(lastLogTerm, lastLogIndex)` election restriction; log repair via
  `nextIndex` back-off with conflict-only truncation; snapshots + InstallSnapshot;
  joint-consensus membership; client linearizability with a dedup session cache;
  and the five safety properties.

### Decisions / notes

- **The Figure-8 rule gets top billing (§7 of the notes).** The naive
  "committed once on a majority" rule is unsafe for entries from prior terms;
  the leader commit-advance must gate on `log[N].term == currentTerm`. This is
  the one subtlety most likely to pass casual testing and silently lose data, so
  the notes pin a **deterministic Figure-8 regression test** as a hard
  requirement for milestone I4.
- **Determinism is designed in from I0.** The notes require an **injected clock
  and injected seeded RNG** in the state machine (no `SystemTime::now()`),
  matching `docs/plan.md` §21, so the `turmoil` simulation suite (I5) is
  reproducible from a fixed seed.
- **redb reuse is already blessed.** ADR 0003 anticipated this phase: "the
  snapshot format becomes the redb file at log index N." I6's snapshot design
  will build on that rather than invent a new on-disk format.
- **Everything traces to a milestone.** The notes end with an implementation
  checklist keyed to I1–I9 and a DoD-to-mechanism table, so each later increment
  has an unambiguous spec to test against.

### Next

- **I1:** ADR 0013 (Raft design — transport, log schema, newtypes) for owner
  sign-off, then scaffold the `brokkr-raft` crate (Transport trait + tonic &
  turmoil impls, redb log schema, `Term`/`LogIndex`/`NodeId` newtypes). The ADR
  needs explicit owner approval **before** any implementation.

## I1 — ADR 0013 + `brokkr-raft` scaffold (§17 task 2)

- **Date:** 2026-07-02
- **Affected:** new `crates/brokkr-raft`; `crates/brokkr-proto` (new
  `raft.proto`); workspace `Cargo.toml` (member + `turmoil` dev-dep).
- **Outcome:** the from-scratch Raft crate's foundation is in place — the pieces
  the consensus state machine (I3–I4) is built on, each with tests in the same
  commit. Owner signed off on the four ADR 0013 decisions via `AskUserQuestion`
  before any code was written.

### Decisions (ADR 0013, all owner-approved)

- **D1 redb schema:** two tables in one `raft.redb`/node — `log` (`u64` index →
  protobuf-encoded `LogEntry`) and `meta` (`&str` → hard state). Entries are
  stored in the same protobuf they take on the wire, so a leader replicates
  stored bytes without re-encoding. `commitIndex`/`lastApplied` stay volatile.
- **D2 transport:** a dedicated `brokkr/v1/raft.proto` (`RaftService`) plus an
  async `Transport` trait. Ships `TonicTransport` (production gRPC) and
  `InMemoryTransport` (deterministic, socket-free) — the latter is the substrate
  for the I2–I4 consensus tests. The tonic-over-`turmoil` fault-injection path is
  I5, once a running node exists to serve; ADR 0013 scopes it there.
- **D3 randomness:** a hand-rolled seeded SplitMix64 PRNG (`rng::Rng`) — **no new
  dependency**, fully reproducible under simulation.
- **D4 concurrency:** a single-task actor / `tokio::select!` event loop
  (documented for I3; no locks on Raft state).

### Notes

- **Persist-before-respond is designed in, proven in I2.** Every `RaftLog`
  mutator commits its redb transaction before returning. The rigorous
  crash-consistency tests and the wiring into the node's reply path are I2; this
  milestone lands the schema and primitives with round-trip + reopen-persistence
  tests.
- **`turmoil` is exercised, not just declared.** An integration test frames
  `brokkr-raft`'s real wire types (protobuf `RequestVote`/reply) over turmoil's
  simulated TCP and asserts a reproducible outcome — so the dev-dep earns its
  place and the I5 suite has a working substrate.
- **Conflict fast-backtrack hint reserved on the wire now.** `AppendEntriesReply`
  carries `conflict_term`/`conflict_index` from the start (cheap to add to the
  proto, used by the I4 log-repair optimization) so we never need a wire change
  for it later.
- **Verified per-crate in WSL2:** `brokkr-raft` is green on `fmt --check`,
  `clippy --all-targets -- -D warnings`, 29 unit + 2 turmoil integration tests, and
  `RUSTDOCFLAGS=-Dwarnings cargo doc`. `brokkr-proto` and the downstream
  `brokkr-control` still compile with the added proto.

### TODOs

- I5: wire the tonic stack over `turmoil` (custom connector + `serve_with_incoming`)
  for partition/delay/reorder fault injection.

### Next

- **I2:** persistent state on redb with the strict persist-before-respond
  discipline and crash tests (kill mid-write, assert no torn vote / consistent
  `(currentTerm, votedFor, log)` on recovery).

## I2 — persistent state + crash consistency (§17 task 2)

- **Date:** 2026-07-03
- **Affected:** `crates/brokkr-raft` (`state.rs` new; `storage.rs`).
- **Outcome:** the Raft hard state is now crash-safe. `HardState`
  (`currentTerm` + `votedFor`) is written **atomically as a unit** through
  `RaftLog::save_hard_state` (one redb transaction), and crash-consistency is
  proven by tests rather than assumed.

### Decisions / notes

- **The torn-vote hazard is the whole point of I2.** I1 wrote `currentTerm` and
  `votedFor` in *separate* transactions. A crash between "bump term to T" and
  "clear vote" could recover as `(T, old-candidate)` — a vote cast in a term the
  node never legitimately voted in, which can produce **two leaders in one term**
  (Election Safety violation, `docs/raft-notes.md` §3, §8). I2 collapses the two
  writes into one atomic commit via `HardState`, so that intermediate state is
  unreachable. `HardState::stepped_to(term)` encodes the "advance term ⇒ clear
  vote" transition in the value itself.
- **Persist-before-respond is now tested two ways.**
  - *Uncommitted writes are invisible* (`uncommitted_write_is_invisible_after_reopen`):
    a write that is begun but never committed (the crash-before-fsync case) leaves
    no trace after reopen — redb rolls it back. This is the deterministic proof of
    the safety model.
  - *Committed writes survive a real crash* (`tests/crash_consistency.rs`): the
    test re-execs its own binary as a child that commits a hard state and then
    calls `std::process::abort()` (a stand-in for power loss). The parent reopens
    and asserts the committed state is intact and uncorrupted.
- **API simplified, not just extended.** `set_current_term` / `set_voted_for`
  were removed (they made the non-atomic hazard *easy to write*); the only way to
  persist hard state now is the atomic `save_hard_state`. `commitIndex` /
  `lastApplied` remain volatile by design (recomputed on restart).
- **Atomic *read*, too (Copilot review catch).** The write is atomic, but the
  first cut of `load_hard_state` composed `current_term()` and `voted_for()` —
  two separate read transactions, so a concurrent `save_hard_state` could
  interleave and hand back a `(term, vote)` pair that was never committed
  together. Fixed to read both keys in one read transaction; a concurrent
  writer/reader stress test (`load_hard_state_reads_a_consistent_pair_under_concurrent_writes`)
  guards it. Also added a `debug_assert` in `HardState::stepped_to` enforcing the
  monotonic-term invariant.

### Verified per-crate in WSL2

`brokkr-raft` green on `fmt --check`, `clippy --all-targets -- -D warnings`,
**37 unit + 4 integration** tests (incl. the subprocess crash test), and
`RUSTDOCFLAGS=-Dwarnings cargo doc`.

### Next

- **I3:** leader election — `RequestVote` handler, the `(lastLogTerm,
  lastLogIndex)` up-to-date comparator (election restriction), randomized
  election timeouts with an **injected clock + seeded RNG**, and the "higher term
  ⇒ step down + clear vote" pre-check wired onto `save_hard_state`. Node logic
  tested against `InMemoryTransport`.

## I3 — leader election (§17 task 2)

- **Date:** 2026-07-03
- **Affected:** `crates/brokkr-raft` (`node.rs` new).
- **Outcome:** the `RaftNode` consensus state machine now elects leaders. It
  implements `RequestVote` (persist-before-respond), the election restriction,
  randomized timeouts, the universal higher-term step-down, majority vote
  counting, and heartbeat-driven election suppression — all proven with eight
  deterministic tests, including a **2–2 split vote resolving in the next term**.

### Decisions / notes

- **Functional core, imperative shell — the key testability decision.** ADR 0013
  D4 mandates a single-task actor with no locks. Rather than build the async
  event loop first (hard to test deterministically without turmoil), I factored
  the node into a **synchronous, single-owner state machine**: `tick(now)` drives
  time and `handle_*(req, now)` drive RPCs, each *returning* the messages to send
  rather than performing I/O. This is the etcd/tikv-style "Ready" pattern. It
  does not contradict D4 — the actor loop (I5) will own exactly this state and
  call these methods — but it lets the tests drive whole clusters by hand with an
  **injected clock + seeded RNG**, no async runtime, fully reproducible. The
  async shell (wiring to `Transport` + a real timer) lands with the simulation
  suite (I5) where simulated time can exercise it.
- **The election restriction is one line, and it's a total order.** "Candidate is
  at least as up-to-date" reduces to `(cand_last_term, cand_last_index) >=
  (our_last_term, our_last_index)` via tuple ordering (§6). Tested across all five
  cases (higher term wins; equal-and-equal grants; equal-term-longer wins;
  equal-term-shorter denies; lower-term denies even if longer).
- **Persist-before-respond, enforced through `save_hard_state`.** Every path that
  changes term or vote — `observe_term` (step down), `start_election` (bump +
  self-vote), granting a vote — writes the atomic `HardState` from I2 *before*
  returning the reply. `observe_term` is the single choke point for the
  "higher term ⇒ follower + clear vote" rule, called at the top of every handler.
- **Heartbeats are the minimal AppendEntries for I3.** A new leader emits empty
  `AppendEntries`; followers reset their election timers on receipt, which is what
  keeps a stable leader from being unseated. The log-consistency check, conflict
  truncation, entry append, commit advance, and the no-op-on-election are all
  explicitly deferred to I4 (marked `TODO(I4)` in `handle_append_entries`).
- **Split-vote test is the proof that randomized timeouts work.** Four nodes,
  two candidates in term 1 splitting the vote 2–2 (no majority); then the
  earliest-expiring node re-campaigns in term 2 and wins. This is exactly the
  §4.1 mechanism, made deterministic by seeding the per-node RNG.

### Verified per-crate in WSL2

`brokkr-raft` green on `fmt --check`, `clippy --all-targets -- -D warnings`,
**47 unit + 4 integration** tests, and `RUSTDOCFLAGS=-Dwarnings cargo doc`.

### Next

- **I4:** log replication — `AppendEntries` consistency check (steps 2–5,
  `docs/raft-notes.md` §5.1), `nextIndex`/`matchIndex` with conflict back-off,
  the leader commit rule gated on `log[N].term == currentTerm`, the start-of-term
  no-op entry, and the mandatory **Figure-8 regression test** (§7).

## I4 — log replication + the Figure-8 rule (§17 task 2)

- **Date:** 2026-07-04
- **Affected:** `crates/brokkr-raft` (`node.rs`, `storage.rs`, `transport.rs`,
  `error.rs`); `crates/brokkr-proto` (additive `match_index` field on
  `AppendEntriesReply`).
- **Outcome:** leaders now replicate their log and advance the commit index
  **safely** — the Figure-8 current-term commit rule is implemented and proven by
  a regression test. This is the safety centerpiece of the whole phase.

### Decisions / notes

- **The current-term commit gate is the one line that matters.**
  `advance_commit_index` walks candidate indices top-down and commits the largest
  `N` that a majority holds **and** whose `log[N].term == currentTerm`. Without
  that second clause a leader would commit a prior-term entry the moment it sits
  on a majority — exactly the Figure-8 data-loss bug. The
  `figure_8_prior_term_entry_not_committed_by_replica_count` test reproduces the
  hazard end-to-end: n0 (term 1) puts entry A on the majority `{n0,n1}` without
  learning it (so A is uncommitted); n1 then wins term 2 and *re-replicates* A (a
  term-1 entry) to the whole cluster; the test asserts `commit_index == 0` at
  that point — A is on a majority but must not commit — and only after n1
  proposes a term-2 entry B that reaches a majority does commit jump to 2,
  sweeping A in indirectly. Remove the current-term clause and this test fails.
- **Conflict-only truncation, never blind truncation.** `append_new_entries`
  keeps any incoming entry we already store at the same term and truncates *only*
  on a genuine term conflict, then appends the rest. A dedicated test
  (`idempotent_append_does_not_truncate_a_matching_suffix`) guards against the
  classic "truncate to prev_log_index then append" bug that a delayed/duplicated
  `AppendEntries` would otherwise use to erase a committed suffix.
- **`matchIndex` reported in the reply, not inferred.** Rather than have the
  leader remember what it sent (fragile under async), the follower reports, on
  success, the highest index it now matches (`prev_log_index + entries.len()`) via
  a new `AppendEntriesReply.match_index` field. The leader sets
  `matchIndex[peer]` from it — correct in both the synchronous test harness and
  the future async driver.
- **Back-off converges via the follower's conflict hint.** On a failed check the
  follower returns the first index of the conflicting term (or `last+1` if its
  log is too short); the leader jumps `nextIndex` there and retries. The
  `lagging_follower_catches_up_via_backoff` test drives an empty follower to a
  three-entry log through this loop.
- **Start-of-term no-op deferred, deliberately.** Raft recommends appending a
  no-op on election so the leader can commit and learn its commit index sooner
  (and to make lease-free reads safe). It is a read-safety / latency
  optimization, **not** required for replication safety, and including it would
  force the Figure-8 test to catch a transient rather than a clean state. It is
  deferred to the linearizable-read work (I8); `become_leader` documents this.

### Verified per-crate in WSL2

`brokkr-raft` green on `fmt --check`, `clippy --all-targets -- -D warnings`,
**56 unit + 4 integration** tests, and `RUSTDOCFLAGS=-Dwarnings cargo doc`.
`brokkr-proto` and the downstream `brokkr-control` compile with the additive
proto field.

### Next

- **I5:** the `turmoil` simulation suite — the **async event-loop shell** that
  wires `RaftNode` to a `Transport` and a real timer, then the tonic-over-turmoil
  transport, driving partitions / message reorder / crash-mid-write against a
  running cluster with a **linearizability** oracle over committed entries.

## I5a — deterministic fault-injection simulator (§17 task 3)

- **Date:** 2026-07-04
- **Affected:** `crates/brokkr-raft` (`tests/simulation.rs`, new).
- **Owner decision:** asked how to build I5 (plan says "turmoil", but
  `RaftNode`'s `std::time` clock doesn't advance under turmoil). Owner chose
  **both** a deterministic simulator *and* the full turmoil async driver, with
  **tonic-over-turmoil** as the transport. I split I5 into **I5a** (this: the
  deterministic simulator — highest safety value, lowest risk, no clock/async
  refactor) and **I5b** (next: the async `RaftDriver` + clock abstraction +
  tonic-over-turmoil).
- **Outcome:** a seeded, in-process discrete-event simulator drives a cluster of
  synchronous `RaftNode`s through message latency/reorder/loss, partitions, and
  crash/restart, and a linearizability oracle proves **State Machine Safety**
  after every step. Five scenarios pass, headlined by a 60-round soak that
  interleaves writes with random faults and never diverges.

### Decisions / notes

- **Why a deterministic simulator, not (only) turmoil.** turmoil runs real async
  code over a simulated network, which is realism I5b will add — but for the
  *safety* oracle, a hand-rolled discrete-event scheduler is both more
  controllable (I can script an exact partition or crash instant) and more
  reproducible (one seed fixes latency, reorder, and the fault sequence), and it
  drives the existing synchronous `RaftNode` **with no clock or async refactor**.
  It reuses the node's `tick`/`handle_*`/`propose` return-the-messages design
  directly.
- **The oracle is committed-prefix agreement.** `assert_no_divergence` checks, for
  every pair of live nodes, that the shorter committed log is a prefix of the
  longer — i.e. no committed index ever holds different commands on two nodes
  (State Machine Safety, `docs/raft-notes.md` §8). It is asserted after *every*
  round of the soak, not just at the end, so a transient divergence cannot slip
  through.
- **Crash = drop volatile, keep disk.** `crash(i)` drops the `RaftNode` (closing
  its redb file) but keeps the temp path; `restart(i)` reopens the *same* file,
  so `currentTerm`/`votedFor`/`log` survive and `commitIndex` is recovered by
  replication — exactly the real crash-recovery contract from I2, now exercised
  under a live cluster.
- **Partitions drop, they don't buffer.** A message between two nodes in
  different partition groups is silently lost at delivery; healing lets *new*
  messages flow. This models a clean network partition and lets the minority side
  demonstrably fail to commit.

### Verified per-crate in WSL2

`brokkr-raft` green on `fmt --check`, `clippy --all-targets -- -D warnings`,
**56 unit + 9 integration** tests (5 new simulation scenarios), and
`RUSTDOCFLAGS=-Dwarnings cargo doc`.

### Next

- **I5b:** the async `RaftDriver` (a `RaftNode` in a tokio task, timer-driven
  `tick`, inbound RPCs over a channel, outbound via a `Transport`), tested on a
  simulated clock. *(As it turned out, the "clock abstraction" was a one-liner —
  `tokio::time::Instant::now().into_std()` — and the real tonic-over-turmoil
  transport was split off into **I5c**; see the I5b entry below.)*

## I5b — async RaftDriver (event-loop shell) (§17 task 3)

- **Date:** 2026-07-06
- **Affected:** `crates/brokkr-raft` (`driver.rs` new, `tests/driver.rs` new,
  `lib.rs`, `Cargo.toml` dev-deps).
- **Outcome:** the synchronous `RaftNode` now runs as a real async task. The
  `RaftDriver` is the imperative shell — one `tokio::select!` loop, no locks —
  and it works end-to-end on a real async runtime, proven deterministically on
  `tokio`'s paused clock (election + commit + minority-partition + heal).

### Decisions / notes

- **Clock: `tokio::time::Instant::now().into_std()`.** `RaftNode.tick`/`handle_*`
  take `now: std::time::Instant`, and the driver sources it from `tokio::time`.
  Under `tokio::time::pause()` (and under `turmoil`) that follows the *simulated*
  clock, so no `Clock` trait or `RaftNode` refactor was needed — the abstraction
  the owner worried about turned out to be a one-liner. The tests advance the
  paused clock in small steps, yielding between them so async message round-trips
  progress; timing is fully reproducible.
- **One writer, no locks.** The node lives inside the select loop; inbound RPCs,
  proposals, tick, and replies-to-our-RPCs are the four arms. Outbounds are sent
  on detached tasks that funnel the peer's reply back into the loop via an
  unbounded channel — so the node is mutated from exactly one place (ADR 0013 D4
  honored at the async layer).
- **`RaftHandle` is both server sink and client.** It implements `RaftRpc`
  (a tonic/`turmoil` server forwards peer RPCs straight to the node) and exposes
  `propose`/`status`. This is what the tonic-over-turmoil server side will wrap.
- **tonic-over-turmoil needs new dependencies — a STOP-AND-ASK.** tonic 0.12 runs
  on hyper 1.0, so bridging `turmoil::net::TcpStream` into tonic's client
  connector and server `serve_with_incoming` requires `hyper-util` (`TokioIo`)
  and probably `tower`/`http-body-util` as **new dev-dependencies**. Per the
  hard "any dep beyond `turmoil` = stop and ask" rule, this milestone ships the
  driver (tested on the paused clock + an in-process transport) and defers the
  real tonic-over-turmoil integration until the owner approves those deps (or
  chooses framed-protobuf-over-turmoil, which needs none and is proven in
  `tests/turmoil_wire.rs`).

### Verified per-crate in WSL2

`brokkr-raft` green on `fmt --check`, `clippy --all-targets -- -D warnings`,
**56 unit + 11 integration** tests (2 new async driver tests), and
`RUSTDOCFLAGS=-Dwarnings cargo doc`.

### Next

- **I5c:** the real **tonic-over-turmoil** transport + a `turmoil` multi-node
  cluster test — pending the dependency decision above.

## I5c — tonic-over-turmoil cluster tests (§17 task 3)

- **Date:** 2026-07-08
- **Affected:** `crates/brokkr-raft` (`tests/turmoil_cluster.rs` new, `lib.rs`
  re-export, `Cargo.toml` dev-deps), workspace `Cargo.toml` + `Cargo.lock`.
- **Outcome:** the gap I5b left open is closed. A real 3-node cluster —
  `RaftDriver`s over the **production `TonicTransport`** — runs with every
  Raft RPC crossing `turmoil`'s simulated network as genuine gRPC/HTTP2.
  Election, replicated commit, a minority-partitioned leader that cannot
  commit, failover to a higher-term leader, and post-heal convergence
  (step-down + overwrite of the uncommitted minority entry) all hold on the
  real wire.

### Decisions / notes

- **The glue is dev-only, ~60 lines, no production change.** Server side: a
  `TurmoilIo` newtype (delegating `AsyncRead`/`AsyncWrite`, implementing
  tonic's `Connected`) feeds `serve_with_incoming` from a
  `turmoil::net::TcpListener` via an accept-pump into a `ReceiverStream`.
  Client side: `Endpoint::connect_with_connector_lazy` with a
  `tower::service_fn` that opens a `turmoil::net::TcpStream` wrapped in
  `hyper_util::rt::TokioIo` (the same pattern as tonic 0.12's UDS example).
  `TonicTransport` and `RaftServiceAdapter` are used exactly as production
  will use them; `RaftServiceAdapter` is now re-exported from the crate root.
- **HTTP/2 keepalive is load-bearing, not decoration.** First run failed:
  after `turmoil::repair`, the healed cluster never re-integrated the deposed
  leader. Cause: turmoil partitions **drop packets silently** (I5a note), so
  an h2 connection that was alive when the partition started never surfaces a
  socket error — tonic pins the dead connection and every RPC on it just
  times out, forever. `http2_keep_alive_interval` + `keep_alive_timeout` +
  `keep_alive_while_idle` on the peer `Endpoint`s makes hyper declare the
  connection dead, and the lazy channel redials. **Carry this into I9:** the
  production HA control plane's peer channels need the same keepalive
  settings, or a real-world partition heals into the same wedge.
- **Deps trimmed from the approved set.** `http-body-util` turned out
  unnecessary; `brokkr-proto`/`prost`/`tonic` were already regular
  dependencies (visible to integration tests). Net new: `hyper-util`
  (`tokio` feature) + `tower` (`util`) in the workspace, and
  `hyper-util`/`tower`/`tokio-stream` as `brokkr-raft` dev-deps.
- **Observation stays out-of-band.** As in the driver tests, `RaftHandle`s in
  a shared registry are used for `status()`/`propose` — only Raft traffic is
  subject to the simulated network, which is exactly what
  `turmoil::partition`/`repair` manipulate. A client-facing RPC surface is
  I8/I9 scope.

### Verified per-crate in WSL2

`brokkr-raft` green on `fmt --check`, `clippy --all-targets -- -D warnings`,
**56 unit + 13 integration** tests (2 new tonic-over-turmoil scenarios), and
`RUSTDOCFLAGS=-Dwarnings cargo doc`.

### Next

- **I6:** snapshots / log compaction (plan §17 task 4) — `InstallSnapshot`
  is already stubbed in the wire types and `RaftRpc`; the driver's
  `install_snapshot` handler currently rejects with "not implemented until
  I6".

## I6 — snapshots & log compaction (§17 task 4)

- **Date:** 2026-07-08
- **Affected:** `crates/brokkr-raft` (`types.rs` `SnapshotMeta`, `storage.rs`
  snapshot persistence, `node.rs` compaction + `InstallSnapshot` both sides,
  `driver.rs` routing + `RaftHandle::compact`, `error.rs` `Snapshot` variant,
  `lib.rs`; tests in `storage.rs`, `node.rs`, `tests/driver.rs`,
  `tests/simulation.rs`).
- **Outcome:** the log no longer grows without bound. The committed prefix
  compacts into a snapshot, a leader catches compacted-away followers up via
  single-shot `InstallSnapshot`, and the whole I5 fault campaign is green
  with `snapshot_threshold = 16` (the plan's exit criterion).

### Decisions / notes

- **Atomicity is the whole game (§III.4):** snapshot metadata + blob + the
  covered prefix's removal commit in **one** redb transaction
  (`RaftLog::compact_to`), so no crash point exists where the prefix is gone
  but the snapshot is not. The receiver's "discard the entire log" path
  (`install_snapshot_replacing_log`) is likewise a single transaction.
- **The node stays sans-IO; the blob comes from the caller.**
  `RaftNode::compact(data)` takes the serialized state machine at exactly
  `commit_index`; `needs_snapshot()` is the trigger the shell polls. The plan
  sketched `Effect::SnapshotRequest`, but the implemented architecture returns
  `Outbound`s rather than effects — a poll + explicit `compact` keeps the
  one-writer discipline without inventing a callback plumbed through the
  event loop. I8's KV wires `snapshot()`/`restore()` to it.
- **Last-`(index, term)` must survive full compaction.** After the whole log
  compacts, `RaftLog::last_index_and_term` falls back to the snapshot
  metadata (one read txn), keeping the election restriction and
  `prev_log_term` at the snapshot boundary correct — proven by a vote test
  on a fully compacted voter.
- **Stale-snapshot guard doubles as the P9 floor.** An inbound snapshot at or
  below `commit_index` (or our own snapshot) is ignored, so applied state
  never regresses; on restart `commit_index` starts at the snapshot index.
- **Single-shot only.** `offset != 0 || !done` → `RaftError::Snapshot`;
  chunking is future work (entries are small KV commands). The driver treats
  that as a peer-protocol error — reply dropped, loop keeps running — while
  local storage errors stay fatal.
- **Oracle under compaction:** the sim's snapshot blob is the committed
  command history (length-prefixed), so `committed(i)` = decoded blob + log
  tail. The linearizability oracle is unchanged in spirit but now spans the
  compacted region; a receiver of `InstallSnapshot` contributes the leader's
  blob as its prefix, which is exactly the restore-then-replay restart path.

### Verified per-crate in WSL2

`brokkr-raft` green on `fmt --check`, `clippy --all-targets -- -D warnings`,
**69 unit + 17 integration** tests (13 new unit; new integration: driver
snapshot catch-up over the switchboard, constant-compaction history
preservation, crashed-follower catch-up via `InstallSnapshot`, and the fault
soak re-run at `snapshot_threshold = 16`), and
`RUSTDOCFLAGS=-Dwarnings cargo doc`.

### Next

- **I7:** membership changes via joint consensus (plan §17 task 5) —
  `EntryPayload::ConfChange`, config applied on append (rolled back on
  truncation, P10), learner catch-up gate, one in-flight change at a time.
  `SnapshotMeta` gains the cluster configuration then.

## I7a — entry payloads & cluster configurations (§17 task 5, groundwork)

- **Date:** 2026-07-08
- **Affected:** `crates/brokkr-proto` (`raft.proto`: `EntryKind`,
  `ClusterConfig`, `LogEntry` fields 4–5), `crates/brokkr-raft` (`types.rs`
  `EntryPayload`/`ClusterConfig` + fallible decode, `transport.rs` entry
  conversion, `lib.rs`; test touch-ups in `node.rs`, `storage.rs`,
  `tests/simulation.rs`).
- **Outcome:** the log can carry configurations. I7 is split like I5 was:
  **I7a** (this) = types + wire/disk encoding + quorum math, no consensus
  behavior change; **I7b** = joint-consensus machinery (config-on-append +
  P10 rollback, dual-majority elections and commits, propose/commit of
  C_old,new → C_new, `SnapshotMeta` gains the config); **I7c** = learners +
  catch-up gate + membership churn in the fault campaign.

### Decisions / notes

- **Backward compatibility is the load-bearing constraint.** `EntryKind`
  rides field 4 with `COMMAND = 0` (the proto3 default), so every entry an
  I6-era node wrote decodes bit-identically as a command — pinned by a test
  that decodes the exact pre-I7 field-1–3 encoding. No migration, no version
  flag.
- **Decode is now fallible.** `From<pb::LogEntry>` became `TryFrom`: an
  unknown kind, a CONFIG entry missing its config, or an invalid node id is
  a `Codec`/`InvalidNodeId` error at the boundary instead of a silently
  mangled entry. `AppendEntries` conversion propagates it.
- **Quorum math is a pure function, tested now.** `ClusterConfig::has_quorum`
  implements §6's rule — strict majority of `voters` AND of `old_voters`
  when joint; learners never count; an empty voter set never has quorum. I7b
  wires it into elections and `advance_commit_index`; landing it first means
  the trickiest arithmetic ships with focused unit tests before any wiring
  can obscure it.
- **The sim oracle ignores non-command payloads** (`LogEntry::command()`
  returns `None` for `Noop`/`Config`): only commands contribute to applied
  history, which is exactly how the I8 state machine will treat them.
- **`Noop` is declared but never produced** — the start-of-term no-op stays
  deferred to the read path (the I4 decision stands); the variant exists so
  the wire format is settled once.

### Verified per-crate in WSL2

`brokkr-proto` and `brokkr-raft` green on `fmt --check`,
`clippy --all-targets -- -D warnings`, tests (**79 unit + 17 integration**;
10 new unit: payload/config round-trips, pre-I7 decode compatibility,
fallible-decode rejections, quorum properties, config-entry disk round-trip),
and `RUSTDOCFLAGS=-Dwarnings cargo doc`.

### Next

- **I7b:** the joint-consensus machinery — track the active config from the
  log (applied on append, rolled back on truncation, P10), replace the fixed
  `peers` set with config-derived replication targets, route elections and
  commits through `has_quorum`, `propose_conf_change` (one in flight;
  C_old,new commits → append C_new; a leader excluded by a committed C_new
  steps down), and `SnapshotMeta` gains the config.

## I7b — joint-consensus membership changes (§17 task 5)

- **Date:** 2026-07-08
- **Affected:** `crates/brokkr-proto` (`InstallSnapshotRequest.config`),
  `crates/brokkr-raft` (`node.rs` `ConfigTracker` + full quorum rewiring +
  `propose_conf_change` + leader stickiness, `types.rs`
  `SnapshotMeta.config` (no longer `Copy`), `storage.rs` snapshot-config
  persistence, `transport.rs` `InstallSnapshot.config`, `driver.rs`
  `propose_conf_change`/`DriverStatus.config`, `error.rs` `ConfChange`).
- **Outcome:** membership is now a property of the log, changed by joint
  consensus exactly per the paper: propose → C_old,new (applied on append,
  dual-majority agreement) → committed → C_new → committed → an excluded
  leader steps down and the remainder carries on.

### Decisions / notes

- **`ConfigTracker` is the P10 mechanism.** Base config (snapshot or
  bootstrap) + every config entry still in the log, ascending. Append pushes,
  truncation drops entries at/after the cut (the rollback), compaction folds
  into the base, and `InstallSnapshot` adopts the carried config. Rebuilt on
  startup by a full log scan (fine at current scale; an index would be an
  optimization, noted for later).
- **The `peers` field is gone.** Replication targets (voters ∪ old voters ∪
  learners − self), vote targets (voters only — learners are never asked),
  election tallies, and `advance_commit_index` all read the active config;
  `has_quorum` filters non-voters, so stray grants can never elect anyone.
- **`advance_commit_index` returns messages now.** A moved commit index can
  demand follow-ups: appending C_new after the joint config commits, or
  stepping down after a committed C_new excludes the leader. `propose` and
  `propose_conf_change` take `now: Instant` because step-down re-arms the
  election timer (same discipline as every other transition).
- **Leader stickiness (thesis §4.2.3) turned out to be REQUIRED, not
  optional.** The driver-level shrink test failed on the first run: after
  C_new committed, the removed server stopped receiving heartbeats, timed
  out repeatedly, and its ever-higher-term `RequestVote`s deposed the
  legitimate leader — the exact disruption §4.2.3 describes. The fix: a
  server that is a leader, or heard from one within `min_election_timeout`,
  disregards `RequestVote` entirely (no term bump, no grant). Liveness is
  preserved because stale leaders still step down via AppendEntries-carried
  terms — pinned by `a_current_leader_ignores_higher_term_request_votes` and
  the contact-goes-stale test. One I3-era test changed expectation
  accordingly.
- **Snapshots carry the config** (plan's `SnapshotMeta.conf`): persisted
  under `snapshot_config`, sent in `InstallSnapshotRequest.config` (field 8),
  adopted by the receiver; restart prefers the snapshot's config over the
  bootstrap peer list. `SnapshotMeta` lost `Copy` — the config is owned.
- **One change in flight.** `propose_conf_change` rejects while the latest
  config is joint or uncommitted, rejects empty/unchanged voter sets, and
  runs entirely leader-side; followers only ever see config *entries*.

### Verified per-crate in WSL2

`brokkr-proto` and `brokkr-raft` green on `fmt --check`,
`clippy --all-targets -- -D warnings`, tests (**87 unit + 18 integration**;
new: P10 append/rollback, add-a-node end-to-end, removed-leader step-down +
re-election, joint-stall without a new-set majority, one-in-flight and input
validation, snapshot-carries-config across restart and `InstallSnapshot`,
leader stickiness ×2, driver conf-change shrink), and
`RUSTDOCFLAGS=-Dwarnings cargo doc`.

### Next

- **I7c:** learners end-to-end — `add_learner`/promotion flow with the
  thesis ch. 4 catch-up gate (a learner joins the voter set only once its
  match index is near the leader's), plus membership churn added to the I5
  fault campaign (the plan's I7 exit criterion).

## I7c — learners & the catch-up gate (§17 task 5, completes I7)

- **Date:** 2026-07-08
- **Affected:** `crates/brokkr-raft` (`node.rs` `new_learner` +
  voters-only-campaign + `propose_add_learner` + the promotion gate,
  `driver.rs` `propose_add_learner`, `Config::catch_up_margin`;
  `tests/simulation.rs` spare slots + churn campaign).
- **Outcome:** the full thesis ch. 4 join flow works — a fresh node enters
  as a non-voting learner, catches up via normal replication (or
  `InstallSnapshot`), and is promoted through joint consensus only once the
  catch-up gate passes. **I7's exit criterion is met**: the fault campaign
  is green with membership churn added to the mix.

### Decisions / notes

- **Only voters campaign.** A node whose active configuration does not name
  it a voter (a learner, or a fully removed server that learned its removal)
  re-arms its election timer and stays quiet. Complements I7b's leader
  stickiness: stickiness protects the cluster from disruptors; this stops
  well-behaved non-voters from becoming disruptors at all.
- **`new_learner` is how nodes join.** Its bootstrap config lists the
  founding voters and itself only as a learner, so a joining node can never
  self-elect off its bootstrap before learning the real membership. The sim
  spawns spares this way; I9's real join flow should too.
- **The gate measures replication distance, not learner status.** A
  promotion is refused iff an added voter's `matchIndex` lags the leader's
  last index by more than `catch_up_margin` (default 256). Adding a voter
  directly to a small/fresh cluster still works (I7b's add-node test is
  unchanged); adding a cold node to a long log is refused with an error
  pointing at the learner flow.
- **Learner additions are single-config entries.** They change no quorum,
  so joint consensus would be ceremony; the one-in-flight rule still
  applies. Promotion (a quorum change) goes through the full
  C_old,new → C_new machinery from I7b, which drops the promoted node from
  `learners` as it enters `voters`.
- **Churn campaign** (`soak_random_faults_with_membership_churn`): 5
  founders + 2 spare slots, threshold 16, 120 rounds of random
  partitions/crashes/restarts interleaved with a retried churn plan
  (add n5 → promote n5 → retire a founder → add n6 → promote n6 → retire
  another). Proposals bounced by the gate/one-in-flight rule are retried
  next round; the linearizability oracle runs after every round. The run
  completes the plan through at least the second add, keeps committing
  writes, and compacts throughout.

### Verified per-crate in WSL2

`brokkr-proto` and `brokkr-raft` green on `fmt --check`,
`clippy --all-targets -- -D warnings`, tests (**91 unit + 19 integration**;
new: learner-never-campaigns, add-learner-without-joint, gated promotion
end-to-end with the promoted voter completing a majority, add-learner
validation, and the churn soak), and `RUSTDOCFLAGS=-Dwarnings cargo doc`.

### Next

- **I7 is complete.** Next is **I8 — Raft-backed KV in the control plane**
  (§17 task 6), which begins with the plan's **MANDATORY stop-and-ask**:
  propose to the owner exactly what replicates through Raft before writing
  any code.

## I8 — the stop-and-ask, resolved (§17 task 6)

- **Date:** 2026-07-09
- **The owner decided** (per the plan's mandatory ask, options + recommendation
  presented and the recommended cut chosen on both):
  1. **What replicates:** action-cache writes and cluster-level configuration.
     **Not** CAS blob bytes (Phase 3 quorum replication owns blob durability)
     and **not** scheduler queue / worker registry / leases (ephemeral by ADR
     design — workers re-register and leases reassign on failover). The I9
     failover story is therefore: builds keep their cache, in-flight jobs
     re-run.
  2. **Reads:** implement **ReadIndex** — the leader confirms leadership with
     a heartbeat round before serving at its commit index. Linearizable reads,
     the textbook mechanism, small now that heartbeats exist.
- **Split:** I8a = the `MetaKv` seam (control-plane refactor, no Raft);
  I8b = the state-machine apply loop + ReadIndex in `brokkr-raft`;
  I8c = `RaftKv` + follower `NotLeader` redirect + 3-node failover tests +
  the `--raft` flag (off ⇒ exactly today's behavior).

## I8a — the `MetaKv` seam (§17 task 6)

- **Date:** 2026-07-09
- **Affected:** `crates/brokkr-control` (`metakv.rs` new; `lib.rs`, `main.rs`).
- **Outcome:** everything the owner chose to replicate now flows through one
  trait. `MetaKv` (get/put/delete/scan_prefix over `Bytes`) with the
  single-node `RedbMetaKv` impl; `MetaKvActionCache<K>` adapts any `MetaKv`
  to the REAPI `ActionCache` trait, and `main` wires the scheduler and the
  `ActionCacheService` through it. Zero behavior change.

### Decisions / notes

- **Namespaced keys in one table** (`ac/<digest-hash>`; cluster config claims
  its own prefix in I8c): one KV instance carries every replicated namespace,
  `scan_prefix` recovers a namespace wholesale, and the I8c snapshot is one
  serialized table.
- **The consumers never see the seam.** Scheduler and service take
  `Arc<dyn ActionCache>` already; only `main`'s wiring changed. Swapping
  `RedbMetaKv` for `RaftKv` in I8c is a one-line change at the same spot.
- **`NotLeader` is deliberately NOT pre-declared.** The review pass showed
  that declaring it alongside a catch-all `From<MetaKvError> for CasError`
  wires the wrong default: the structured leader hint would flatten into a
  storage-error string and the I8c redirect would silently ship broken. The
  conversion is instead an exhaustive match — when I8c adds the variant, the
  compile breaks right there until the redirect (gRPC `FAILED_PRECONDITION`
  + leader hint in metadata) gets a real propagation path.
- **Storage location changes** from `action_cache.redb` (table
  `action_results`) to `meta.redb` (table `meta_kv`). Existing cached action
  results are not migrated — it is a cache; it refills. `RedbActionCache`
  stays in `brokkr-cas` for its other users.
- **The contract suite is reusable:** `metakv_contract_suite<K: MetaKv>` is
  `pub(crate)` in the test module; I8c runs the same suite against `RaftKv`,
  which is the plan's "contract suite runs against both impls" requirement.

### Verified per-crate in WSL2

`brokkr-control` and `brokkr-cas` green on `fmt --check`,
`clippy --all-targets -- -D warnings`, tests (**94 lib + all integration
suites** for control; 5 new: contract suite on `RedbMetaKv`, reopen
persistence, concurrency limit, AC round-trip through the KV, namespace
isolation of `list_entries`), and `RUSTDOCFLAGS=-Dwarnings cargo doc`.

### Next

- **I8b:** the `StateMachine` apply loop in the `brokkr-raft` driver
  (apply committed entries / snapshot / restore — the hooks I6 left for
  "the shell") and **ReadIndex** in the core (leadership confirmation
  round, read served at the confirmed commit index once applied).

## I8b — state-machine apply loop + ReadIndex (§17 task 6)

- **Date:** 2026-07-10
- **Affected:** `crates/brokkr-proto` (`raft.proto`: `seq` on
  AppendEntries request/reply), `crates/brokkr-raft` (`node.rs` no-op +
  ReadIndex, `driver.rs` `StateMachine` + apply loop + `read_index`,
  `transport.rs`, `lib.rs`; every index-sensitive test updated).
- **Outcome:** the shell finally *applies* what consensus commits, and the
  leader serves linearizable reads. Both halves of the I8 read/write story
  that `RaftKv` (I8c) plugs into are now in place.

### Decisions / notes

- **The start-of-term no-op is no longer deferred.** I4 postponed it as a
  read-path optimization; the read path arrived. `become_leader` appends
  `EntryPayload::Noop` (the variant I7a reserved), so a fresh leader
  commits an entry of its own term without waiting for client traffic and
  the Figure-8 rule sweeps inherited entries in immediately. The Figure-8
  regression test was rewritten around it — election by votes only, no-op
  replication held back, then released — and now demonstrates commitment
  arriving *exclusively* through the new term's entry.
- **ReadIndex confirmation is seq-proven.** Every `AppendEntries` carries a
  monotonic per-leader `seq`, echoed in the reply. A read registers with
  `confirm_seq = ae_seq + 1` and counts only acks whose echoed seq is at or
  past it: a delayed reply to a pre-registration request can never confirm
  current leadership (tested head-on). Quorum is `has_quorum` over the
  active config — joint-consensus-correct for free. Any term-current
  response counts, success or log-mismatch: both prove the peer accepted
  our leadership when it answered.
- **Reads floor at `term_start_index`.** The read index is
  `max(commit_index, term_start_index)`: ReadIndex is unsafe until an entry
  of the current term commits (paper §8), and flooring at the no-op makes
  the wait implicit — the driver serves the read once applied catches up,
  which cannot happen before the no-op commits.
- **Leadership loss fails reads.** Pending reads live in `LeaderState`;
  every step-down path drains them into `Err(NotLeader)` results. A deposed
  leader never serves a read it could not confirm (driver-level test kills
  a partitioned leader mid-read).
- **The driver applies, snapshots, and restores.** `StateMachine`
  (apply/snapshot/restore) is owned by the driver: committed entries apply
  in order exactly once; `last_applied` floors at the snapshot (P9);
  startup and leader-installed snapshots restore the machine; and
  compaction is now automatic past `Config::snapshot_threshold`, using the
  machine's own serialized state — the I6 "shell supplies the blob"
  contract, fulfilled by the shell itself.
- **Index churn was the honest cost:** every election now inserts a no-op,
  so ~15 tests' exact indices shifted by one (or two after re-elections),
  and the sim oracle compares history-to-history instead of
  history-length-to-commit-index (no-ops produce no output). Failover
  tests learned that a healed deposed leader can force a re-election —
  asserts there are now relative where the absolute number was incidental.

### Verified per-crate in WSL2

`brokkr-proto` and `brokkr-raft` green on `fmt --check`,
`clippy --all-targets -- -D warnings`, tests (**99 unit + 21 integration**;
new: five node-level ReadIndex tests — post-registration-ack-only
confirmation, term-start floor, step-down failure, single-voter immediate,
follower rejection — and two driver-level tests: end-to-end linearizable
read with apply catch-up, deposed-leader read failure), and
`RUSTDOCFLAGS=-Dwarnings cargo doc`.

### Next

- **I8c:** `RaftKv` in `brokkr-control` — a `MetaKv` impl that proposes
  prost-encoded commands through the driver, reads via
  `read_index()` + the applied machine, returns
  `MetaKvError::NotLeader { leader }` on followers (the exhaustive
  `From` conversion in `metakv.rs` will force the redirect design, as
  planned), runs the MetaKv contract suite, and lands the 3-node
  kill-the-leader failover test plus the `--raft` flag (off ⇒ exactly
  today's behavior).

## I8c — `RaftKv` (§17 task 6, completes I8)

- **Date:** 2026-07-10
- **Affected:** `crates/brokkr-raft` (`driver.rs` `propose_committed` +
  term-checked apply waiters), `crates/brokkr-cas` (`error.rs`
  `NotLeader`), `crates/brokkr-control` (`raftkv.rs` new; `metakv.rs`,
  `services/action_cache.rs`, `main.rs` `--raft`, `lib.rs`;
  `tests/raft_kv_cluster.rs` new).
- **Outcome:** the from-scratch Raft carries real control-plane traffic.
  `RaftKv` satisfies the same `MetaKv` contract as the redb store, writes
  survive killing the leader, and followers redirect with a structured
  leader hint. I8's exit criterion holds: `--raft` off is byte-for-byte
  today's behavior.

### Decisions / notes

- **Write acks mean applied, not appended.** `RaftHandle::propose` resolves
  on append (unchanged); the new `propose_committed` resolves when the
  entry is committed *and applied*, so a subsequent leader-local read
  observes the write. The apply waiter records the proposal's term and
  verifies it against the entry actually applied at that index: if a new
  leader truncated and overwrote it, the caller gets `NotLeader` — never a
  false success. A snapshot-jump over a waiter resolves it as
  outcome-unknown rather than guessing.
- **The materialized map is in-memory, and that is correct.** The plan
  sketched a "redb-backed materialized state machine"; the durable layer is
  the Raft **log + snapshots** (both redb), and I8b's restore-plus-replay
  rebuilds the map on restart. A second durable store would just be a cache
  of the log with its own consistency questions. Deviation noted here per
  the working rules.
- **The I8a forcing function fired.** Adding `MetaKvError::NotLeader`
  broke the exhaustive `From<MetaKvError> for CasError` at compile time —
  exactly the designed effect — which drove `CasError::NotLeader` and the
  `ActionCacheService` mapping (gRPC `FAILED_PRECONDITION`, leader identity
  in `x-brokkr-leader` metadata). The hint carries the leader's node id;
  I9's peer wiring maps ids to addresses at the service edge.
- **`--raft` is single-voter in I8c.** It exercises the full write/read
  path (election, no-op, propose, apply, ReadIndex, snapshots) on one node;
  peers/failover/bring-up are I9's `--node-id`/`--raft-peer` work. Reads on
  followers are leader-served in I8c (`NotLeader` redirect) — follower
  reads at the read index are a possible I9+ optimization, not a
  correctness need.

### Verified per-crate in WSL2

`brokkr-raft`, `brokkr-cas`, `brokkr-control` green on `fmt --check`,
`clippy --all-targets -- -D warnings`, tests (**98 control lib + all
integration suites**; new: the `MetaKv` contract suite against `RaftKv`,
write-visible-to-read, `KvMachine` snapshot round-trip incl. truncated-blob
tolerance, `cas_status` redirect mapping, and the 3-node cluster tests —
write → kill leader → linearizable read from the new leader, and
follower write/read refused with the leader hint), and
`RUSTDOCFLAGS=-Dwarnings cargo doc`.

### Next

- **I9 — HA control plane + the DoD runs** (§17 tasks 7–8): `--node-id` /
  `--raft-peer id=addr` flags and the `RaftService` server wired into
  `brokkr-control`, three-process bring-up docs + script, the real-process
  kill-the-leader test measuring **time-to-new-leader < 2 s** (DoD 1),
  partition semantics via turmoil (DoD 2), and the **1M-op certification
  run** with seeds recorded in this journal (DoD 3).

## I9a — HA control plane + DoD 1 (§17 task 7)

- **Date:** 2026-07-11
- **Affected:** `crates/brokkr-control` (`main.rs` flags + multi-node
  wiring; `tests/raft_ha_cluster.rs` new),
  `docs/operations/running-a-cluster.md`, `scripts/run-cluster.sh`.
- **Outcome:** three real `brokkr-control` processes form a Raft cluster,
  and **DoD 1 holds with a 7× margin**: kill the leader → a survivor
  accepts writes in **291.9 ms / 288.2 ms** (two runs; budget 2 s), with
  the pre-kill write read back linearizably from the new leader.

### Decisions / notes

- **The I5c keepalive lesson shipped where it mattered.** Peer channels in
  `main.rs` set `http2_keep_alive_interval` + `keep_alive_timeout` +
  `keep_alive_while_idle` — exactly what the I5c journal entry flagged for
  I9. Connect/RPC timeouts (500 ms / 1 s) keep a dead peer from wedging
  replication.
- **Election seeds derive from the node id** (hashed): identical seeds
  would synchronize randomized timeouts across nodes and produce repeated
  split votes on a symmetric bring-up.
- **Leader discovery is client-shaped.** The DoD test finds the leader by
  attempting the write on every node and following `FAILED_PRECONDITION`
  refusals — the I8c redirect surface, exercised as a client would.
- **Peer links are plaintext this phase.** The worker/client planes keep
  their mTLS/JWT posture; raft-plane mTLS is a follow-up, documented in
  `--raft-listen`'s help and the ops doc.
- **DoD 2 (partition semantics) is certified in the simulation layers**, as
  the plan prescribes (real-process partitioning needs root/netns — not
  attempted): the minority side cannot commit and heals consistently in
  `simulation.rs::minority_partition_cannot_commit_and_heals_consistently`
  and (over real gRPC) in
  `turmoil_cluster.rs::grpc_cluster_survives_leader_partition_and_heals`.
- **Not yet in I9a:** worker failover across control-plane leader changes
  (workers pin one endpoint today) and the full `brokk run`-after-kill
  end-to-end — that plus the **1M-op certification run (DoD 3)** are the
  remaining I9 work.

### DoD scoreboard

1. **< 2 s failover: PROVEN** — 291.9 ms / 288.2 ms on real processes
   (`raft_ha_cluster.rs`, WSL2, debug build).
2. **Partition semantics: PROVEN** in deterministic simulation + turmoil
   (citations above).
3. **1M-op certification: pending** (I9b/I9c). *(Proven in I9c — see that
   entry's scoreboard: 1,000,000 ops, zero divergence, seed 20260730.)*

### Verified per-crate in WSL2

`brokkr-control` green on `fmt --check`,
`clippy --all-targets -- -D warnings`, all test suites, `RUSTDOCFLAGS`
docs, plus the `#[ignore]` real-process DoD run above.

### Next

- **I9b:** worker endpoint rotation on control-plane failover + the full
  three-control + worker + `brokk run` kill-the-leader end-to-end; then
  the **1M-op certification run** (release mode, full fault mix +
  membership churn + tiny snapshot threshold), seeds and timings recorded
  here (DoD 3).

## I9b W1+W2 — the redirect becomes actionable (§17 task 7)

- **Date:** 2026-07-30
- **Affected:** `crates/brokkr-cas` (`error.rs`), `crates/brokkr-control`
  (`metakv.rs`, `raftkv.rs`, `services/action_cache.rs`, `main.rs`;
  `tests/raft_kv_cluster.rs`).
- **Outcome:** a client refused by a follower now learns *where* the leader
  is, not merely its name. I9b lands in three PRs (W1+W2 here, then
  SDK/worker failover, then the real-process end-to-end); this is the first.

### Decisions / notes

- **I8c shipped a redirect nothing could follow.** `x-brokkr-leader` carried
  a node id, and a node id is not a dial target — `--raft-peer id=host:port`
  addresses are the *raft plane*, not the client listener. The I8c entry
  deferred this to "I9's peer wiring"; I9a did the peer wiring and left the
  hint alone, so the gap survived two milestones while looking closed. Worth
  remembering that a structurally-correct error can still be operationally
  useless.
- **The plan's W1 was unimplementable as written, and the fix is smaller than
  the plan.** §VII.3 W1 had *each* node propose its own `cfg/nodes/<id>`
  record. A follower cannot propose, so on a fresh cluster no follower could
  ever publish and the records would never exist. But the asymmetry is the
  answer: the only address a redirect ever needs is the **leader's**, and the
  leader is by definition able to propose it. So only the leader publishes,
  and only its own record. `cfg/nodes/` stays a map of every node that has
  ever *held leadership*, which is exactly the set worth redirecting to.
- **Rejected: putting client addresses in `--raft-peer`.** It would work
  today — all members already must agree on full membership — and needs no
  proposal at all. Rejected because joint-consensus membership (I7) can add a
  node at runtime, and flags cannot describe a node that did not exist at
  launch. Consensus state belongs in consensus.
- **The hint is read without ReadIndex, deliberately.** `published_addr`
  reads the applied map directly. Demanding linearizability for a routing
  hint would be pointless (leadership can change the instant after we answer)
  and impossible anyway — the reader is a *follower*, which cannot serve a
  linearizable read. A stale address costs the client one failed dial; a
  consensus round trip would cost every redirect.
- **Each hint is emitted independently.** There is a real window between an
  election and the new leader's record committing, where the id is known and
  the address is not; the redirect must still go out (the client falls back to
  its configured endpoints). Two named tests pin it: an id with no address
  emits only `x-brokkr-leader`, and an address that will not parse as a header
  value does not suppress the id.
- **A wildcard bind is refused, not published.** `0.0.0.0:7878` is a binding
  instruction; advertising it hands every redirected client a guaranteed
  failure. Under `--raft` with a wildcard `--listen`, startup fails naming
  `--advertise-addr` — and only under `--raft`, so single-node operators are
  not made to name themselves for a record nobody reads.
- **The I8a forcing function fired twice more.** Adding a field to
  `NotLeader` broke the exhaustive `From<MetaKvError> for CasError` *and* the
  I8c follower test's destructuring pattern — both at compile time, both in
  the right place. The dead free-standing `kv_err` fell out naturally once
  every call site needed `&self` to resolve the address.

### Verified per-crate in WSL2

`brokkr-cas` + `brokkr-control` green on `fmt --check`,
`clippy --all-targets -- -D warnings`, and tests (**103 control lib**, up from
98; new: `node_record_keys_are_namespaced_and_disjoint_from_the_ac_prefix`,
`the_leader_publishes_its_address_and_any_replica_resolves_it`,
`a_corrupt_or_empty_node_record_yields_no_address_rather_than_panicking`,
`a_known_leader_with_no_published_address_emits_only_the_id`,
`an_unparseable_leader_address_still_yields_the_id_hint`,
`advertise_addr_resolution_refuses_only_unusable_wildcards`; extended:
`not_leader_maps_to_failed_precondition_with_a_leader_hint` and
`raft_kv_cluster.rs::follower_write_is_refused_with_a_leader_hint`, which now
asserts a follower resolves the leader's *published* address).

**Host baseline, established this milestone:** a full
`cargo test --workspace --no-fail-fast` runs 46 suites and exactly one fails —
`brokkr-sandbox --test evil_seccomp_caps`, the 6 tests needing a real kernel
for seccomp argument filters. That set is now the explicit gate: anything else
failing is a regression, not the host.

### Next

- **I9b W3+W4+W5:** `BrokkrClient::connect_any` + redirect-following on
  `x-brokkr-leader-addr` (bounded hops, last-known-leader cached), repeatable
  `--control` on `brokk` and on `brokkr-worker` with endpoint rotation and
  re-registration, and D1 — the best-effort post-execution action-cache write
  (`warn` + `uncached_results_not_leader` counter + a not-cached marker in
  execution metadata; every non-`NotLeader` error still fails the RPC).

## I9b W3–W5 — client & worker failover (§17 task 7, completes I9b)

- **Date:** 2026-07-30
- **Affected:** `crates/brokkr-control` (`scheduler.rs`,
  `services/execution.rs`; `tests/leader_redirect.rs` new),
  `crates/brokkr-worker` (`endpoint.rs` new, `worker.rs`, `main.rs`),
  `crates/brokkr-sdk` (`redirect.rs` new, `client.rs`),
  `crates/brokkr-cli` (`main.rs`).
- **Outcome:** an HA control plane is now usable from the outside. A client
  pointed at any node reaches the leader; a worker outlives the node it
  attached to; and a completed action is never discarded because the write
  landed on a follower.

### Decisions / notes

- **D1 shipped as the owner chose, with the objection built in as work.** The
  recorded worry about best-effort was that it hides a routing problem as a
  cache-hit-rate problem. So the degradation is observable in three places —
  a counter (`uncached_results_not_leader`), a `warn` carrying digest and
  leader, and `ExecuteResponse.message` so the *client* can tell "ran and
  cached" from "ran, not cached". And it is narrow: **only `NotLeader`
  degrades**; a `Redb` or throughput error still fails the RPC, with a named
  test for exactly that. Best-effort must not become best-ignored.
- **No retry on the internal write, deliberately.** This node is not the
  leader and the only store reachable from it is the one that just refused.
  Forwarding to the leader is §VII.2 option (b) — a different design, now on
  the deferred list — not a retry.
- **The pure-policy discipline paid for itself twice.** Extracting
  `rotation_plan(len, attempt)` and `redirect::classify(status, hops)` as pure
  functions, tested exhaustively, surfaced three bugs that would otherwise
  have shipped: a `1 << n` overflow panic after ~64 reconnect cycles, a
  modulo-by-zero taking the worker down on an empty endpoint set, and a
  redirect silently downgrading `https` to `http` when the hint carried a bare
  `host:port`. None of those are reachable from a happy-path integration test.
- **A redirect is identified by metadata, not by the status code.** The
  scheduler already returns `FAILED_PRECONDITION` for "no eligible worker", so
  matching on the code alone would have retried an unrelated failure as a
  redirect — against a *different* node, which is worse than failing.
- **The first reconnect cycle has zero delay.** A leader kill should cost a
  worker milliseconds: the survivors are up *now*. Backoff belongs to the case
  where a whole cycle has failed and nothing is listening. A session that
  registered resets the counter, or a worker running for hours would inherit an
  old outage's backoff and crawl through a failover it should sail through.
- **What D1 cost the redirect path, stated plainly.** Because `Execute` no
  longer fails on `NotLeader`, and the CLI never calls the ActionCache
  directly (the scheduler does cache lookups server-side), **no in-tree client
  RPC returns a leader redirect through `brokk run`.** The redirect surface
  that matters is the REAPI one an external client (Bazel) drives, so this
  increment adds `BrokkrClient::{get_action_result, update_action_result}`
  with redirect-following and tests them against real servers — rather than
  leaving a tested policy with no consumer and calling that "clients follow
  redirects".
- **`tonic::Status` is not `Clone`,** and the leader hints live in its
  metadata. Reporting a redirect we could not follow therefore rebuilds the
  status preserving code, message **and** metadata; dropping the metadata
  would discard the one detail an operator needs.

### Verified per-crate in WSL2

`brokkr-cas`, `brokkr-control`, `brokkr-worker`, `brokkr-sdk`, `brokkr-cli`
green on `fmt --check`, `clippy --workspace --all-targets -- -D warnings`,
`RUSTDOCFLAGS=-Dwarnings cargo doc --no-deps`, and
`cargo test --workspace --no-fail-fast` (**48 suites**; the only failures are
the 6 `evil_seccomp_caps` tests that need a real kernel for seccomp argument
filters — the recorded host gate).

New tests: 2 scheduler tests for D1 (result returned uncached and counted; a
non-`NotLeader` failure still fails the RPC), 6 for `rotation_plan` (first-cycle
immediacy, capped monotonic backoff, overflow, single endpoint, empty set,
jitter), 2 for endpoint pairing (`--worker-control` paired positionally; a
partial list rejected), 6 for `redirect::classify`/`hint_to_url` (hinted
refusal, unroutable hint, unrelated failures, hop-budget termination, empty
metadata, scheme preservation), and 3 integration tests in
`leader_redirect.rs` driving real `ActionCacheService` servers (a client on a
follower reaches the leader for both read and write; an unroutable hint
surfaces the original refusal with its metadata intact; a self-referential
hint terminates on the hop budget instead of looping).

### Next

- **P3 / W6:** the real-process end-to-end — three control nodes + a worker +
  `brokk run`, kill the leader, assert the next run succeeds within budget and
  that the pre-kill *leader-routed* write is a cache hit afterwards (under D1
  a follower-routed build caches nothing, so that write must go through the
  leader for the assertion to mean anything).
- Then **I9c** — the 1,000,000-operation certification, the last open
  definition-of-done line.

## I9b W6 — the real-process end-to-end (§17 task 7, completes I9b)

- **Date:** 2026-07-30
- **Affected:** `crates/brokkr-control` (`scheduler.rs`;
  `tests/raft_ha_e2e.rs` new), `crates/brokkr-sdk` (`client.rs`),
  `crates/brokkr-cli` (`main.rs`), `docs/operations/running-a-cluster.md`.
- **Outcome:** three real `brokkr-control` processes, three workers, and
  `brokk run` — SIGKILL the leader and the next build succeeds in
  **133.4 ms**, with the pre-kill cache entry intact and served by a node that
  was a follower when the write happened.

### The two bugs this test found

Both were invisible to every in-process test, and both are the reason W6 was
worth writing rather than declaring I9b done after W5.

- **An HA control plane needs a worker on every node.** The first run died on
  `no eligible worker for action platform`. One worker attaches to exactly one
  control node, and the worker registry is per-node and **ephemeral by ADR
  0008–0010 design** — deliberately not replicated through Raft, because
  workers re-register and leases reassign on failure. So on a three-node
  control plane with one worker, two nodes can execute nothing: a build routed
  there fails however healthy Raft is. §VII.1's gap list named "workers pin one
  endpoint" but missed "one worker only serves one node" — a different problem
  with a different fix. Now documented as a requirement in
  `docs/operations/running-a-cluster.md`, not just fixed in the test.
- **D1 had been implemented on half the path.** With workers everywhere the
  follower-routed build *still* failed:
  `action cache get: not the metadata leader (leader hint: Some("control-2")
  at Some("127.0.0.1:40249"))`. Decision D1 made the post-execution **write**
  best-effort; the pre-execution **lookup** still failed hard. Reads are
  leader-served (I8c ReadIndex), so on a follower the lookup returns
  `NotLeader` and the build died *before the action ever ran* — strictly worse
  than the write case D1 was written for, because the caller asked to execute
  an action, not to be told which node they reached. A lookup that cannot be
  served is now a **miss**; every other error still fails the RPC, so a storage
  fault cannot masquerade as a miss and silently re-execute cached work.
  `a_follower_cache_lookup_is_a_miss_not_a_failure` pins it.

That same error message is also the first end-to-end proof of I9b W1/W2: the
hint carried both the leader's id **and** a dialable address, resolved by a
follower from its own applied `cfg/nodes/` record.

### Decisions / notes

- **The test is built around D1 rather than despite it.** A follower-routed
  build caches nothing, so the write whose survival is asserted must go through
  the **leader** — asserting a cache hit on a follower-routed build would pass
  for the wrong reason, or fail for one that is by design. The sequence is
  therefore: follower-routed build succeeds *and reports not-cached* → the same
  action through the leader is cached → SIGKILL → a new action succeeds within
  budget → the leader-routed action is a cache hit from a survivor.
- **`RunOutcome` was dropping `ExecuteResponse.message`,** so `brokk` could not
  tell a user their build was not cached. D1's "the client is told" was true of
  the REAPI response and false of the CLI. Fixed here, because the e2e's first
  assertion is unobservable without it — a good example of an end-to-end test
  forcing an honest surface.
- **Budget is 5 s, not 2 s,** and deliberately so: DoD 1's two-second bound is
  about electing a leader (proven in `raft_ha_cluster.rs`), while this measures
  election **plus** worker rotation, re-registration, dispatch and a real
  process execution. The measured 133 ms leaves the budget looking generous;
  it is sized for a loaded CI host, not for this number.

### Verified in WSL2

`cargo test -p brokkr-control --test raft_ha_e2e -- --ignored --nocapture`:
**passed in 4.30 s**, leader was node 2, first successful post-kill build
**133.408551 ms** (budget 5 s), pre-kill leader-routed action confirmed a cache
hit afterwards. Workspace gates green on CI's exact commands (`fmt --check`,
`clippy --workspace --all-targets --locked -D warnings`, `doc --all-features
--locked`, `test --workspace --all-targets --locked`, `test --doc`), with only
the 6 known `evil_seccomp_caps` failures that need a real kernel.

### Next

- **I9c — the 1,000,000-operation certification** (DoD 3), the last open
  definition-of-done line: extend the existing `simulation.rs` campaign, count
  client operations rather than sim steps, run the full fault mix with
  membership churn and `snapshot_threshold = 16`, and record seeds, ops/sec,
  wall clock and peak RSS here.

## I9c — the 1,000,000-operation certification (§17 task 8, DoD 3)

- **Date:** 2026-07-30
- **Affected:** `crates/brokkr-raft/tests/simulation.rs`
  (`certification_one_million_ops`), `docs/plan.md` (§11.1).
- **Outcome:** **DoD 3 is proven.** One million client operations under the
  full fault mix with membership churn and continuous compaction, **zero
  divergence**.

### The run

```text
cargo test -p brokkr-raft --release --test simulation -- --ignored --nocapture certification
[cert] DONE ops=1000000 rounds=32216 commit=906330 wall=3060.8s rate=327/s
       seed=20260730 snapshot_threshold=512
```

| Metric | Value |
|---|---|
| Client operations | **1,000,000** |
| Rounds | 32,216 |
| Final commit index | 906,330 |
| Wall clock | 3,060.8 s (51 min; 52:20 including compile) |
| Mean rate | 327 ops/s |
| Peak RSS | 3,058,272 kB (**2.92 GiB**) |
| CPU | 870.7 s user + 403.6 s system |
| Seed | 20260730 (`BROKKR_CERT_SEED`) |
| Snapshot threshold | 512 (`BROKKR_CERT_SNAPSHOT_THRESHOLD`) |
| Oracle interval | 100,000 ops |
| Host | WSL2, `--release` |

No seed has ever failed, so there is no new fixture to add. Had one failed it
would have become a permanent **non-`#[ignore]`d** regression test.

### Decisions / notes

- **Every axis at once, which is the part that was never covered.** I5 varied
  faults, I6 varied compaction, I7 varied membership — each holding the others
  still. This run does all three simultaneously: latency jitter, partitions,
  heals, crashes, restarts, learner→voter promotions, and compaction firing
  roughly 1,770 times per node. The 906,330 final commit index against
  1,000,000 accepted proposals is the fault injection visibly doing its job —
  proposals accepted by a leader that then lost leadership never committed,
  exactly as Raft requires.
- **The harness's snapshot design is O(n²), and it is a test artifact.** The
  first attempt ran at `snapshot_threshold = 16` and decayed from 462 to
  188 ops/s by 400k ops, projecting past two hours. Cause: `Sim::maybe_compact`
  calls `committed(i)` (decodes the whole snapshot, walks the log) and then
  `encode_history` (re-encodes all of it), **per node, every time
  `needs_snapshot()` fires** — at threshold 16 that is a full O(history) pass
  every 16 committed entries, ≈437k passes over the run.
  This is the *oracle's* design, not Raft's: the harness stores the entire
  applied history inside each snapshot blob so histories can be compared
  directly. A real state machine snapshots bounded **state** — `KvMachine`
  snapshots a map.
- **So the constant was fixed, not the requirement.** Threshold 512 keeps
  compaction and `InstallSnapshot` running continuously (~1,770 compactions per
  node) while cutting the quadratic term 32×. The other campaigns keep
  threshold 16 at their smaller scales, so the aggressive-compaction path stays
  covered. **The operation count was never touched** — it is verbatim in the
  plan's definition of done, and lowering it to make a number look better would
  have made the certification worthless.
- **The rate curve is the evidence the diagnosis was right.** After the fix:
  496 → 396 → 373 → 366 → 363 → 359 → 355 → 347 → 334 ops/s — a decline that
  *flattens*, consistent with a much smaller quadratic term plus the O(history)
  oracle firing ten times. Before the fix it fell steeply and kept falling.
- **2.92 GiB peak RSS is the same artifact seen from the memory side:** seven
  nodes each holding a snapshot blob containing the entire million-command
  history. Real deployments do not do this; the certification harness does.
- **Deferred, and now specific:** replace the history-blob oracle with a
  **hash-chain** oracle (chain(k) = H(chain(k-1) ‖ cmd_k)) — O(1) per entry
  instead of O(history), which would make both the time and the memory linear.
  Deliberately not done during the certification: rewriting the oracle to
  certify against it would mean certifying against an unproven oracle.

### DoD scoreboard — all three lines proven

1. **< 2 s failover: PROVEN** — 291.9 ms / 288.2 ms on real processes
   (`raft_ha_cluster.rs`); and 133.4 ms kill→completed `brokk run` end to end
   (`raft_ha_e2e.rs`).
2. **Partition semantics: PROVEN** — minority cannot commit and heals
   consistently, in deterministic simulation and over real gRPC via turmoil.
3. **1,000,000 operations under fault injection with zero divergence:
   PROVEN** — the run above.

### Next

- **I9d:** raft-plane mTLS (ADR 0011 is already amended with the design).
- **I10:** the Phase 5 wrap-up and §11 exit-criteria review.

## I9d — raft-plane mTLS (§17 task 7, ADR 0011 amendment)

- **Date:** 2026-07-30
- **Affected:** `crates/brokkr-control` (`main.rs`;
  `tests/raft_mtls_cluster.rs` new), `docs/architecture/0011-auth.md`,
  `docs/operations/running-a-cluster.md`.
- **Outcome:** the Raft peer plane is mutual-TLS. Three nodes over mTLS still
  elect and replicate, and a node whose certificate is signed by an untrusted
  CA **cannot join or commit anything**.

### Decisions / notes

- **Why this was not left as a follow-up.** I9a shipped the peer plane in
  plaintext on the reasoning that it runs on a trusted network. That is a
  *deployment assumption*, not a security property — and the asymmetry with
  the other two planes is what settles it: on the client and worker planes the
  worst case is unauthorized access to *cache* data, but `AppendEntries` on
  the peer plane **appends to the replicated log**. An unauthenticated peer
  port is a write path into consensus itself. Shipping "HA control plane" with
  that open, mentioned only in a flag's help text, was not defensible.
- **Three planes, and the peer plane is deliberately the odd one.** Client =
  JWT bearer (a tenant identity), worker = mTLS (a machine identity), peer =
  **mTLS mutual-only, no JWT**. A peer is not a tenant and has no user
  identity to carry; adding a bearer token there would be one more thing to
  misconfigure while authenticating nothing extra. Written up in the ADR 0011
  amendment rather than left implicit.
- **`resolve_raft_tls` is a pure function, and that is the point.** The
  failure this guards against is *silent until a peer first tries to
  replicate* — a half-configured plane starts happily and dies at handshake
  time. Making the decision pure means all six partial combinations are tested
  without binding a socket, and each test asserts the error **names the
  missing flag** rather than saying "invalid configuration". Fully configured
  with `--raft` off is also refused: those flags would otherwise silently do
  nothing.
- **Both directions, one code path.** The `--raft-listen` server gets
  `client_ca_root` (which makes the client certificate mandatory in tonic
  0.12), and the outbound peer channels get a `ClientTlsConfig` carrying both
  the CA and this node's identity — reusing the worker-plane plumbing rather
  than a parallel TLS implementation, because a second implementation is
  exactly how the half-configured states issue #139 closed get reintroduced.
  The peer URL scheme follows the posture (`https` vs `http`) so a mismatch
  cannot be constructed by hand.
- **Proving the good path is not enough.** The load-bearing test is
  `a_peer_with_an_untrusted_certificate_cannot_join`: three nodes presenting
  `badworker.pem` (signed by `badca`) while verifying against `ca.pem` must
  **never commit a write**, because no quorum can form when every
  `AppendEntries` fails the handshake. Without it the plane could be
  configured-but-not-enforcing — which is precisely the bug issue #139 found
  on the worker plane, where the CA was loaded but the client certificate was
  never actually required.
- **Plaintext stays available and now says what it costs.** Local development
  (`scripts/run-cluster.sh --ha`) still runs unencrypted, but startup logs a
  warning naming the consequence — anyone who can reach the port can append to
  the replicated log — instead of a neutral "TLS disabled".

### Verified in WSL2

`cargo test -p brokkr-control --test raft_mtls_cluster -- --ignored`:
**2 passed in 12.42 s** (mTLS cluster elects, replicates and reads back; the
untrusted-CA cluster commits nothing). Unit tests: 8 in the `brokkr-control`
binary including `raft_plane_tls_is_all_or_nothing` (all three flags, none,
each of the six partial combinations, and configured-with-`--raft`-off).
Workspace green on CI's exact five commands, with only the 6 known
`evil_seccomp_caps` failures that need a real kernel.

### Next

- **I10:** the Phase 5 wrap-up and §11 exit-criteria review. All three
  definition-of-done lines are proven; nothing in §17 remains open.

## Phase 5 wrap-up & exit-criteria review (I10)

Phase 5 replaced the control plane's embedded metadata store with a Raft
implementation written from the paper — no external Raft crate, per CLAUDE.md
rule 10 — and stood up a control plane that survives losing a node. Shipped
across PRs #120–#172 and ADR 0013.

### What shipped (by §17 task)

| Task | Capability | Milestones |
|---|---|---|
| 1 | `docs/raft-notes.md` — the implementation reference, from the extended paper | I0 |
| 2 | ADR 0013 + `brokkr-raft`: crash-safe hard state on redb, leader election, log replication, the Figure-8 commit rule | I1–I4 |
| 3 | Deterministic fault-injection simulator, async `RaftDriver`, tonic-over-turmoil cluster tests | I5a–I5c |
| 4 | Snapshots + log compaction + `InstallSnapshot` | I6 |
| 5 | Joint-consensus membership changes + learners with a catch-up gate | I7a–I7c |
| 6 | `MetaKv` seam, state-machine apply loop, ReadIndex, `RaftKv` | I8a–I8c |
| 7 | HA control plane; actionable leader redirects; client & worker failover; real-process e2e; raft-plane mTLS | I9a, I9b, I9d |
| 8 | The 1,000,000-operation certification | I9c |

### §17 Definition of Done — all three lines proven

1. ✅ **Kill the leader → a new one in < 2 s.** 291.9 ms / 288.2 ms on three
   real processes (`raft_ha_cluster.rs`); **133.4 ms** from SIGKILL to a
   completed `brokk run` end to end, cache intact (`raft_ha_e2e.rs`).
2. ✅ **Partition → minority stops accepting writes; rejoin → consistent.**
   `simulation.rs::minority_partition_cannot_commit_and_heals_consistently`
   and, over real gRPC, `turmoil_cluster.rs::grpc_cluster_survives_leader_partition_and_heals`.
   Real-process partitioning needs root + netns and was deliberately not
   attempted — stated plainly rather than implied.
3. ✅ **1,000,000 operations under fault injection, zero divergence.**
   `ops=1000000 rounds=32216 commit=906330 wall=3060.8s rate=327/s
   seed=20260730` (I9c).

### Exit criteria (`docs/plan.md` §11)

1. **Rustdoc on all public APIs** — ✅ `cargo doc --all-features -D warnings`
   green in CI. One slip was caught this phase (a public doc linking a private
   const) and the doc gate is now run as its own step, since clippy and tests
   both pass while it fails.
2. **Unit tests ≥80% on logic-heavy code** — ✅ by inspection for `node.rs`
   (election, replication, Figure-8, ReadIndex), `driver.rs`, `storage.rs`,
   `raftkv.rs`, `metakv.rs`, plus the pure policy functions added in I9b
   (`rotation_plan`, `redirect::classify`, `resolve_raft_tls`). **No coverage
   tool is wired into CI**, so this is an inspection claim rather than a
   measured number — the same honest caveat Phase 4 recorded, and still worth
   closing with `cargo-llvm-cov` one day.
3. **≥1 integration test per capability** — ✅ `crash_consistency.rs`,
   `driver.rs`, `simulation.rs`, `turmoil_cluster.rs`, `turmoil_wire.rs`,
   `raft_kv_cluster.rs`, `raft_ha_cluster.rs`, `raft_ha_e2e.rs`,
   `leader_redirect.rs`, `raft_mtls_cluster.rs`.
4. **Tracing spans on new code paths** — ✅ `RaftService` handlers, the driver
   apply loop, `RaftKv` get/put/delete/scan, and the I9b redirect path.
5. **Retrospective** — ✅ below.

### Retrospective

- **What the paper hid.** The Figure-8 rule is stated in a paragraph and costs
  a day to get right; the *start-of-term no-op* it implies is barely
  mentioned, and deferring it (as I4 did) quietly makes ReadIndex unsafe until
  I8b adds it back. The paper is a specification, not an implementation plan.
- **What the deterministic simulator caught that turmoil could not.** Seeded
  reproducibility: a failing interleaving becomes a fixture. Turmoil exercises
  the real tonic stack but a failure there is a story, not a seed.
- **What turmoil caught that the simulator could not.** The keepalive lesson
  (I5c): a partition that silently drops packets leaves an h2 connection
  "alive" forever, so a healed cluster never re-integrates the peer. No
  pure-core simulator models that, because it is a property of the transport.
  I9a shipped the fix into production wiring precisely because I5c wrote it
  down.
- **Index churn is a test-design lesson.** Adding the start-of-term no-op
  shifted ~15 tests' absolute indices by one. Tests asserting *relative*
  progress survived; tests asserting exact numbers had to be rewritten. The
  absolute numbers were incidental detail masquerading as specification.
- **The I8a forcing function fired three times.** Declaring the
  `From<MetaKvError> for CasError` conversion as an exhaustive match — instead
  of a catch-all — broke the build at exactly the right place when I8c added
  `NotLeader` and again when I9b added `leader_addr`, including inside a test's
  destructuring pattern. A deliberate compile error is worth more than a
  comment asking future readers to remember something.
- **Real processes found what no in-process test could.** The I9b e2e exposed
  two defects on its first two runs: an HA control plane needs a worker on
  *every* node (the registry is per-node and deliberately unreplicated), and
  decision D1 had been implemented on only half its path — a follower's cache
  *lookup* still failed builds before the action ran. Both were invisible to
  every in-process test, because those pass `skip_cache_lookup` or use a
  working store.
- **The certification's first failure was in the test, not the system.** A 1M
  run decayed from 462 to 188 ops/s and projected past two hours; the cause was
  the harness storing the entire applied history inside every snapshot blob,
  making compaction O(history) and the run O(n²). Fixing the *constant* rather
  than the *requirement* mattered: lowering the operation count to make a
  number look better would have made the certification worthless.
- **CI caught a defective assertion inside DoD line 2's own proof — after the
  phase was declared done.** The wrap-up PR, which changes no code, failed
  `cargo test` on **aarch64 only**: *a minority-partitioned leader cannot
  advance its commit index: left: 3, right: 2*. Two defects, both in the test.
  The assertion tested a **proxy, not the property** — an index of 3 arises
  equally from "committed its own doomed entry" (a real safety violation) and
  from "stepped down and legitimately learned the new leader's entry" (correct
  Raft), so it **failed on correct behaviour while being unable to prove the
  incorrect one**. And the test was **nondeterministic despite turmoil**: its
  registry was a `HashMap`, whose per-process randomized iteration order
  changed the polling order and hence the interleaving — which is why x86_64
  passed and aarch64 failed on byte-identical code.
  Fixed by `BTreeMap` for deterministic iteration plus an assertion that splits
  the cases (still leading ⇒ commit index frozen; stepped down ⇒ must have
  adopted the higher term), leaving the unconditional post-heal assertions as
  the real safety proof. Verified over 12 consecutive runs, since one green run
  says nothing about a per-process nondeterminism. `build_with_rng` would also
  pin turmoil's latency jitter but needs `rand`, which ADR 0013 deliberately
  avoided — left as a follow-up rather than quietly added.
  The lesson is not "tests flake". A test that usually passes can be **testing
  the wrong thing**, and re-running until green would have buried that
  permanently inside the evidence for a definition-of-done line.

- **Pure policy functions paid for themselves repeatedly.** Extracting
  `rotation_plan`, `redirect::classify` and `resolve_raft_tls` and testing them
  exhaustively caught five bugs no happy-path integration test reaches: a
  `1 << n` overflow panic, a modulo-by-zero, an https→http downgrade on
  redirect, a redirect cycle that hung, and half-configured TLS that would have
  failed only at handshake time.

### Deferred (with reasons)

| Deferred | Why it is safe to defer |
|---|---|
| Pre-vote | Election disruption after a partition heals is a liveness annoyance, not a safety hole; terms already step down correctly. |
| Leadership transfer | Graceful handoff is an operability nicety; kill-and-elect is proven at 291 ms. |
| Fast log backup (conflict index/term hints) | `nextIndex` back-off is correct, just chattier on a long divergence. |
| Chunked `InstallSnapshot` | Snapshots are a serialized map today; chunking matters when state outgrows one message. |
| Follower reads at the read index | Reads are leader-served and linearizable; follower reads are throughput, not correctness. |
| Dynamic peer discovery / auto-join | Membership is flag-configured plus joint consensus; discovery is deployment convenience. |
| Raft metrics dashboard | Spans and counters exist; the dashboard is presentation. |
| D1 alternatives — admission-time refusal, follower→leader forwarding | The chosen best-effort path is correct and observable; both alternatives change the control plane's active/passive posture and deserve their own decision. |
| Durable scheduler state (job history, leases through Raft) | Ephemeral by ADR 0008–0010 design; replicating it would cost consensus round trips on every heartbeat. |
| **Hash-chain oracle for the simulator** | The history-blob oracle is correct but O(history); a chain hash would make the certification linear in time *and* memory. The only deferred item that is a known inefficiency rather than a missing feature. |

### Phase 5 status: **complete.** All three DoD lines proven; no §17 task open.
