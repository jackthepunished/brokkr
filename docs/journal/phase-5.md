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
  `clippy --all-targets -D warnings`, 29 unit + 2 turmoil integration tests, and
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

`brokkr-raft` green on `fmt --check`, `clippy --all-targets -D warnings`,
**37 unit + 4 integration** tests (incl. the subprocess crash test), and
`RUSTDOCFLAGS=-Dwarnings cargo doc`.

### Next

- **I3:** leader election — `RequestVote` handler, the `(lastLogTerm,
  lastLogIndex)` up-to-date comparator (election restriction), randomized
  election timeouts with an **injected clock + seeded RNG**, and the "higher term
  ⇒ step down + clear vote" pre-check wired onto `save_hard_state`. Node logic
  tested against `InMemoryTransport`.
