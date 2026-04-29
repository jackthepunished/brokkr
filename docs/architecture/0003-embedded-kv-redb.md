# 0003 — Embedded KV store: redb

- **Status:** accepted
- **Date:** 2026-04-30
- **Deciders:** Brokkr maintainers

## Context

The control plane and CAS need a durable, transactional key-value store
for metadata: action cache entries, worker registry rows, tenant config,
and CAS reference counts (`docs/plan.md` §6.6, §10). Phase 0–4 use a
single-node embedded store; Phase 5 replaces it with a from-scratch Raft
KV (separate ADR will follow when Phase 5 begins). This decision is about
the Phase 0–4 store only.

Requirements:

1. **ACID transactions.** Action-cache writes must be all-or-nothing —
   the action key, the result blob digests, and the metadata update must
   commit atomically.
2. **Pure Rust.** Cross-compilation to Linux x86_64 and aarch64 must not
   require a C/C++ toolchain on the build host (`docs/plan.md` §7).
3. **Embedded, single-process.** No external server to operate during
   Phase 0–4.
4. **Predictable point-read latency.** Action-cache hot path is a
   `digest(Action)` lookup; performance target is p99 <5 ms
   (`docs/plan.md` §23).
5. **Stable on-disk format.** We will live with this format until Phase 5;
   we do not want to migrate Phase 1 data because the storage engine
   broke compatibility.
6. **Path to Raft.** When Phase 5 lands, the embedded store's tables
   should be reusable as the Raft state machine's snapshot format.

## Decision

Use **redb 2.x** (`redb` crate) as the embedded KV store across the
control plane and CAS during Phases 0–4. Tables encode protobuf-serialized
values; keys are typed via redb's `TableDefinition`.

Concrete usage already in tree:

- `crates/brokkr-cas/src/redb_backend.rs` — CAS blob index + refcounts.
- `crates/brokkr-cas/src/action_cache.rs` — action cache table.
- Future: `workers.redb`, `tenants.redb` in `brokkr-control` per
  `docs/plan.md` §10.

Workspace dependency is pinned to `redb = "2"` in the root `Cargo.toml`.

## Alternatives considered

- **sled.**
  - Pros: pure Rust; log-structured; tree API is ergonomic; supports
    subscribers/watchers out of the box.
  - Cons: has been on `0.34` for years; the planned 1.0 rewrite has
    stalled multiple times; non-trivial history of crash-recovery bugs;
    upstream maintenance cadence is uncertain. For a store that holds
    every action-cache entry we ever care about, "uncertain crash
    recovery" is disqualifying.

- **RocksDB** (via `rust-rocksdb`).
  - Pros: battle-tested in TiKV, CockroachDB, Kafka; LSM is a good fit
    for write-heavy workloads; rich tuning surface.
  - Cons: C++ dependency — adds a system toolchain requirement that
    breaks the "pure Rust, no C deps in the data plane" invariant;
    multi-minute clean compile on cold CI; ~10 MB binary inflation; LSM
    write-amp tradeoffs are wrong for our workload (action-cache writes
    are infrequent, reads dominate); tuning surface is a footgun for a
    solo project.

- **LMDB** (via `heed` or `lmdb-rkv`).
  - Pros: extremely fast point reads (mmap zero-copy); decades of
    production use; small code surface.
  - Cons: C dependency; mmap semantics are subtle on WSL2 and over
    network filesystems (the Brokkr dev environment is WSL2); writer
    serialization is single-process global; database file size is
    pre-allocated and fragile to grow; harder to fuzz.

- **SQLite** (via `rusqlite` or `sqlx`).
  - Pros: the most boring, most-tested embedded store on earth; SQL is
    universally understood; great tooling.
  - Cons: SQL surface is overkill for typed KV; we would store
    bincode-encoded protobuf in `BLOB` columns, which throws away the
    SQL win; C dependency; row-level overhead on point lookups vs. a
    pure B-tree; schema migrations become a problem we did not need to
    have.

- **Hand-rolled bincode-on-disk.**
  - Pros: zero dependencies; full control.
  - Cons: we would be reinventing crash recovery, transactions, and a
    page cache; this is a tar pit; the educational core of the project
    is Raft (Phase 5), not a B-tree (Phase 0).

- **Use Raft KV from day one** (`raft-rs`, `openraft`, etc.).
  - Cons: violates CLAUDE.md hard rule #10 (no external Raft crate);
    delays Phase 1 by months; inverts the learning order — we want to
    *use* a simple store first so we know what the Raft store needs to
    replace.

## Consequences

### Positive

- **Pure Rust.** Cross-compilation to Linux aarch64 from x86_64 (and to
  macOS/Windows for the CLI) needs no system protobuf or C++ toolchain.
- **B-tree predictability.** Action-cache point reads are O(log n) with
  bounded variance — the right shape for our hot path.
- **Single-file databases.** Backups, snapshots, and `scp`-style
  operational moves are trivial; one file per logical store.
- **MVCC reads** allow long readers without blocking writers — fits the
  control-plane pattern of frequent reads, occasional writes.
- **Format stability** — redb 2.x explicitly commits to read-compatibility
  for older databases. Phase 1 data will still open in Phase 4.
- **Migration path to Raft.** Each redb table maps cleanly to a Raft
  state-machine column family; the snapshot format becomes "the redb
  file at log index N." We will not need to redesign the storage shape
  to bolt on Raft — only the replication layer above it.

### Negative

- **Single-process only.** No client/server mode; the metadata store
  *is* the control-plane process. Acceptable through Phase 4; Phase 5
  changes this by design.
- **Smaller community than RocksDB or SQLite.** Bug reports against
  redb are answered, but ecosystem expertise is shallower. Mitigated
  by: redb is small enough to read end-to-end if we hit a bug.
- **No replication, no async I/O.** redb is synchronous and local. We
  call it from `tokio::task::spawn_blocking` to keep the runtime
  responsive. Documented as a wrapper pattern in `brokkr-common`.
- **No SQL.** Ad-hoc operational queries ("which tenant owns this
  action cache row?") require either a small CLI subcommand or a
  redb-aware inspection tool. Acceptable; we ship `brokk admin` for
  this.

### Neutral

- **Encoding** is protobuf for values that travel over the wire (so
  the disk format matches the RPC format) and bincode for values that
  never leave the node (refcounts, internal indices). Documented in
  `crates/brokkr-cas/src/redb_backend.rs`.
- **One file per logical store** (`action-cache.redb`, `workers.redb`,
  `tenants.redb`, `refcount.redb`) per `docs/plan.md` §10. Different
  stores have different durability requirements and are easy to
  reason about when they are physically separate.
- **Phase 5 supersession.** This ADR will be superseded by the Raft KV
  ADR when Phase 5 begins. The supersession is planned, not a failure.

## References

- redb: <https://github.com/cberner/redb>
- redb file-format stability notes:
  <https://github.com/cberner/redb/blob/master/docs/design.md>
- `docs/plan.md` §6.6 (Metadata Store), §7 (Technology Stack),
  §10 (Storage Layout), §17 (Phase 5 Raft).
- `crates/brokkr-cas/src/redb_backend.rs` — current usage.
- TiKV's RocksDB experience (counter-example at scale):
  <https://github.com/tikv/tikv>
- sled status discussion:
  <https://github.com/spacejam/sled/issues>
