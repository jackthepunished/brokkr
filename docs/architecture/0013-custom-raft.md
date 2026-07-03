# 0013 — Custom Raft consensus for the metadata store

- **Status:** accepted
- **Date:** 2026-07-02
- **Deciders:** Brokkr maintainers
- **Supersedes:** 0003 (embedded KV: redb) — for the *replication* layer only;
  redb remains the local storage engine underneath Raft.

## Context

Phase 5 (`docs/plan.md` §17) makes the control-plane metadata store highly
available by replacing the single-node embedded store with a **from-scratch
Raft** implementation. CLAUDE.md rule 10 forbids importing any existing Raft
crate (`raft-rs`, `openraft`, …); the whole point of the phase is to implement
consensus from the paper. `docs/raft-notes.md` is the reading-derived spec.

This ADR fixes the design decisions that the crate scaffold (milestone I1) and
everything after it depend on, so later milestones do not re-litigate them.
The four questions this ADR answers were signed off by the owner before any
code was written:

1. How is the Raft log + hard state persisted on redb?
2. How do nodes talk to each other (wire protocol + transport abstraction)?
3. Where does the randomness for election timeouts come from, given the
   "no new dependency beyond `turmoil`" constraint?
4. What is the per-node concurrency model?

The definition of done (`docs/plan.md` §17) is: kill the leader → new leader in
< 2 s; partition → minority stops accepting writes, rejoin → consistent; 1M
operations under fault injection with zero divergence. Every decision below is
made in service of *provable, deterministically-testable* safety, because that
DoD is verified by simulation (§21 `turmoil`, fixed seeds).

## Decision

A new library crate **`brokkr-raft`** implements Raft from scratch. It sits in
the DAG as `brokkr-raft → brokkr-proto → brokkr-common`, and `brokkr-control`
will depend on it (I8). Four design decisions:

### D1 — Persistence: two redb tables, protobuf-encoded log entries

Each node owns one `raft.redb` file with two tables:

```text
raft.redb
├─ TABLE "log":  u64  → &[u8]   // protobuf-encoded LogEntry { term, index, command }
└─ TABLE "meta": &str → &[u8]   // hard state: "current_term", "voted_for",
                                //  and (from I6) "last_included_index/term"
```

Log entries are stored **protobuf-encoded** — identical to how they travel on
the wire in `AppendEntries` — so a leader replicates stored bytes without
re-encoding. Hard-state values (`currentTerm`, `votedFor`) live in the `meta`
table. `commitIndex` and `lastApplied` are **volatile** (recomputed after
restart, per `docs/raft-notes.md` §3). The **persist-before-respond** rule
(hard state and log durably committed before any dependent reply) is the core of
milestone I2 and its crash tests.

### D2 — Transport: dedicated `raft.proto` + an async `Transport` trait

A new `brokkr/v1/raft.proto` defines `RaftService` with `RequestVote`,
`AppendEntries`, and `InstallSnapshot`, versioned alongside the other
`brokkr.v1` protos in `brokkr-proto`. `brokkr-raft` defines an async
`Transport` trait (via `async-trait`) that the node calls to reach peers, with:

- **`TonicTransport`** — production gRPC over real TCP.
- a **turmoil path** — the same tonic client/server run over `turmoil`'s
  simulated TCP (custom connector + `serve_with_incoming`), so the deterministic
  simulation suite (I5) exercises the *real* transport stack under injected
  partitions, delays, and reordering rather than a mock.

Internal Rust request/reply types decouple the state machine from generated
prost types; `From`/`TryFrom` conversions bridge the two and are unit-tested for
round-trip fidelity.

### D3 — Randomness: a hand-rolled seeded PRNG, no new dependency

Election-timeout jitter (150–300 ms) uses a ~15-line **SplitMix64** PRNG that
lives in `brokkr-raft::rng`, seeded explicitly and **injected** into each node.
No cryptographic quality is required for timer jitter; full control over the
seed makes `turmoil` runs reproducible from a fixed seed. This adds **zero**
dependencies, honouring the "any dependency beyond `turmoil` is a stop-and-ask"
constraint.

### D4 — Concurrency: a single-task actor / event loop

Each `RaftNode` **owns all of its state** and runs one `tokio::select!` event
loop over: inbound RPCs, election/heartbeat timer ticks, and client proposals,
all delivered over `mpsc` channels. There is **no lock** on Raft state. This is
the most testable shape and is maximally friendly to deterministic simulation —
there are no lock interleavings for the linearizability checker to reason about.

Determinism is a first-class constraint throughout: the **clock is injected**
(no `SystemTime::now()` in the state machine) alongside the seeded RNG, matching
`docs/plan.md` §21.

## Alternatives considered

### D1 alternatives (persistence)
- **Two tables, bincode-encoded entries.** Cheaper Rust-side, but every
  `AppendEntries` would re-encode bincode → protobuf. Rejected: store-as-sent is
  simpler and avoids a re-encode on the hot replication path.
- **Single table, enum-tagged keys.** Fewer tables, but mixes log and metadata
  durability in one B-tree and muddies the "snapshot = the redb file at index N"
  story from ADR 0003. Rejected for clarity.

### D2 alternatives (transport)
- **`Transport` trait with only a turmoil impl in I1**, deferring `raft.proto` +
  tonic to a later milestone. Rejected: the loop plan scopes "tonic & turmoil
  impls" to I1, and testing the sim path over the *real* tonic stack is more
  valuable than a mock.
- **A pure in-memory `SimTransport`** (channels, no sockets) for simulation.
  Rejected as the *primary* sim substrate because it would not exercise the real
  gRPC codec/framing; it may still appear as a fast unit-test helper, but the
  DoD-grade fault injection runs over tonic-on-turmoil.

### D3 alternatives (randomness)
- **`rand` + `rand_chacha`.** The standard, audited choice, but two new direct
  dependencies beyond the pre-approved `turmoil` — a stop-and-ask the owner
  declined in favour of the zero-dependency PRNG.

### D4 alternatives (concurrency)
- **`Arc<Mutex<RaftState>>` shared across tasks.** Simpler task wiring, but lock
  interleavings hurt determinism and make the safety argument harder. Rejected.

## Consequences

### Positive
- **Determinism by construction.** Injected clock + seeded PRNG + single-owner
  state ⇒ `turmoil` runs are byte-for-byte reproducible from a seed, which is
  what makes the 1M-op DoD auditable.
- **Real-stack fault testing.** The turmoil path runs the actual tonic transport,
  so the simulation catches codec/framing/timeout bugs a mock would hide.
- **Store-as-sent log.** No re-encode on replication; the on-disk format is the
  wire format.
- **Clean DAG.** `brokkr-raft` depends only on `brokkr-proto`/`brokkr-common`;
  the control plane depends on it, no cycles.
- **redb continuity.** ADR 0003's "snapshot = the redb file at log index N"
  survives — I6 snapshots build on the same engine.

### Negative
- **We own the correctness.** No battle-tested library to fall back on; every
  safety property must be pinned by a test (the Figure-8 regression test in I4
  is mandatory). This is the deliberate educational cost of rule 10.
- **turmoil-on-tonic glue.** A custom connector + `serve_with_incoming` wiring is
  fiddly and is carried as scaffolding until the node exists (I2–I4).
- **Hand-rolled PRNG.** We are responsible for its statistical adequacy for timer
  jitter (it is fine for this; it would *not* be fine for anything security- or
  hash-relevant, which it is never used for).

### Neutral
- **Supersession of ADR 0003 is partial.** redb stays as the local engine; only
  the *single-node* assumption is replaced by Raft replication. ADR 0003's status
  is updated to note this.
- **Internal vs. proto types.** Maintaining `From`/`TryFrom` between them is a
  small tax paid for keeping the state machine free of generated-code shapes.

## References

- `docs/raft-notes.md` — the from-paper spec these decisions implement.
- **Raft (extended)**, Ongaro & Ousterhout, 2014 — <https://raft.github.io/raft.pdf>
- ADR 0003 (redb) — the storage engine this builds on.
- `docs/plan.md` §17 (Phase 5 tasks + DoD), §21 (testing / `turmoil`).
- `turmoil` — <https://github.com/tokio-rs/turmoil> (deterministic network sim).
- Prior art read, not copied (rule 10): etcd `raft/`, TiKV `raftstore`
  (`docs/plan.md` §28 Tier 4).
