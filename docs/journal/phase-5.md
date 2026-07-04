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
  `tick`, inbound RPCs over a channel, outbound via a `Transport`) + a clock
  abstraction so the node runs on turmoil's sim clock, wired to the real
  **tonic-over-turmoil** transport (ADR 0013 D2), with a multi-node turmoil
  cluster test under partitions/latency.
