# Changelog

All notable changes to Brokkr will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Phase 0 bootstrap: Cargo workspace, 9 crates, toolchain pin to Rust 1.85,
  rustfmt/clippy/deny configuration, root README, CONTRIBUTING, LICENSE
  (Apache-2.0), CHANGELOG, CODE_OF_CONDUCT, justfile.
- `CLAUDE.md` operating manual at the repo root.
- `docs/plan.md` — project source of truth.
- Vendored REAPI v2 + supporting googleapis protos in `brokkr-proto`,
  compiled via `tonic-build` with a vendored `protoc` (no system dependency).
- `brokk version` and `brokk init` (stub) subcommands, with git SHA + rustc
  + target triple embedded at build time.
- GitHub Actions CI (`fmt`, `clippy -D warnings`, `test`, `build --release`)
  on Linux x86_64 and aarch64.
- ADR 0001 — Rust everywhere.
- `docs/journal/phase-0.md` — Phase 0 retrospective.

### Changed
- MSRV bumped from 1.78 → 1.85 during bootstrap (transitive deps require
  edition 2024).

## Phase 1 (in progress)

### Added
- `brokkr_common::Digest` — sha256 newtype with strict shape validation,
  content verification, `Display`/`FromStr` round-trip, and unit tests.
- `brokkr-cas`: async `Cas` trait + `InMemoryCas` backend implementing
  `find_missing_blobs` / `batch_update_blobs` / `batch_read_blobs` with
  per-entry digest verification.
- `brokkr-cas`: `RedbCas` on-disk backend (single-file `redb` database,
  `spawn_blocking` around sync redb txns) with persistence-across-reopen and
  digest-mismatch tests.
- `brokkr-cas`: `ActionCache` trait + `RedbActionCache` backend storing
  prost-encoded REAPI `ActionResult` keyed on action digest hash; tests for
  miss, roundtrip, overwrite, and persistence-across-reopen.
- `brokkr-control`: REAPI `ContentAddressableStorage`, `ActionCache`, and
  `Capabilities` services bound to the redb backends; `Execution` service
  stub returning `Unimplemented` until the worker dispatch path lands.
  `brokkr-control` binary now boots a tonic gRPC server on a configurable
  `--listen` and `--data-dir`.
- `brokkr-control` integration tests: in-process server + tonic clients
  exercising capabilities, CAS roundtrip, and action-cache miss-then-hit.
- `brokkr.v1.worker.proto` — internal worker dispatch protocol
  (`WorkerService.Register` + bidi `Stream`).
- `brokkr-control` `Scheduler`: single-queue, single-worker job dispatcher
  bridging REAPI `Execute` to the internal worker stream; consults and
  writes the action cache; only caches `exit_code == 0`.
- `brokkr-control` `WorkerServiceImpl`: claims the job receiver and pumps
  jobs out / results in over the bidi stream.
- `brokkr-control` `ExecutionService`: streams `google.longrunning.Operation`
  results carrying an `ExecuteResponse` payload.
- `brokkr-worker`: `runner` (plain-process spawn capturing stdout/stderr)
  and `worker` control loop that registers, opens the bidi stream, runs each
  job, uploads stdout/stderr blobs to CAS, and reports `JobResult`.
- `brokkr-sdk`: `BrokkrClient::connect` + `run_command` that builds an
  Action, uploads it to CAS, calls Execute, and decodes the streamed result.
- `brokk run [-- argv...]` subcommand: connects to the control plane, runs
  the command, forwards stdout/stderr, and exits with the action's exit code.
- `brokkr-control` `tests/end_to_end.rs`: full in-process cluster (server +
  worker) running `/bin/echo "hello world"` end-to-end and verifying that the
  second invocation hits the action cache.
- Tracing spans on the Phase 1 hot path: `client::execute` (SDK),
  `control::dispatch` (scheduler), and `worker::run_action` (worker), each
  recording action digest / job id / cache hit / exit code as the action
  flows through the layers (plan §13.9).
- `brokkr-sdk`: `run_command` now issues a `FindMissingBlobs` precheck and
  only uploads the Action / Command / input-root entries that the CAS does
  not already have. Closes plan §13.7 ("uploads any missing input blobs")
  and removes a fixed cost from the cache-hit path.
- `brokkr-control` `tests/phase1_dod.rs`: Phase 1 DoD assertions —
  `one_hundred_iterations_deterministic` (200 RPCs, 100 distinct
  commands, miss-then-hit each, `#[ignore]`-soak) and
  `cache_hit_faster_than_miss` (median-of-10 timing comparison).
  Shared cluster fixture moved to `tests/common/mod.rs`.

## Phase 2 (in progress)

### Added
- `docs/phase-2-plan.md` — detailed Phase 2 implementation plan (threat
  model, re-exec runner architecture, public API, per-subsystem designs,
  evil-action matrix, M1–M9 milestones, CI / WSL2 notes).
- `brokkr-sandbox::host_check` — Linux host-compatibility probes (kernel
  version, unprivileged userns, cgroup v2, brokkr.slice writable, seccomp
  presence, `memory.peak`, `/proc/self/setgroups`) returning a structured
  `Report` with pass/warn/fail outcomes.
- `brokkr-worker --check-host` — runs the host probes, prints the
  checklist, exits 0 iff the sandbox is functional on this host
  (warnings allowed). Plan §10.3.
- `scripts/install-cgroup-slice.sh` — one-shot host setup that creates
  `/sys/fs/cgroup/brokkr.slice`, chowns it to the target user, and
  delegates the cpu/memory/pids/io controllers. Idempotent.
- `docs/journal/phase-2.md` — Phase 2 journal, started with the M1 entry.
- `brokkr-sandbox` public API: `Sandbox`, `SandboxConfig`, `SandboxOutcome`,
  `ExitStatus`, `ResourceAccounting`, `SandboxTimings`, `SandboxError` —
  full type surface that subsequent milestones light up incrementally.
  `SandboxConfig` is also the IPC payload between host and runner
  (serde JSON over fd 3).
- `brokkr-sandboxd` runner binary inside the `brokkr-sandbox` crate.
  M2 reads the config from fd 3, optionally chdirs to `workdir`, and
  `execvpe`s the action — Phase-1 parity inside the new re-exec model.
  Namespace / cgroup / seccomp setup is added by M3–M8.
- Host-side spawn uses `pipe2(O_CLOEXEC)` for the config pipe so the
  runner's inherited copy of the write end auto-closes on `execve`,
  letting `read_to_end(fd 3)` see EOF. `pre_exec` clears
  `FD_CLOEXEC` on fd 3 even when `pipe2` happens to return the read
  end already at fd 3.
- `brokkr-sandbox/tests/sandbox_smoke.rs` — seven end-to-end smoke
  tests (echo, /bin/false, missing argv0, empty argv error, env
  passthrough, workdir, timings populated).
- `nix` workspace dep (`features = ["fs", "process", "user"]`) for the
  raw Linux primitives the sandbox needs.
- M3: user namespace + mount namespace + `pivot_root` in the runner.
  `runner/userns.rs` does the unprivileged `0 <host_uid> 1` mapping
  (with the `setgroups`-deny gotcha); `runner/mount.rs` makes `/`
  recursively private, builds a tmpfs rootfs, applies
  `RootfsSpec.{ro_binds, tmpfs, symlinks}`, and pivots into it.
  `RootfsSpec` gained a `symlinks` field plus an `is_empty()` helper
  (`Default` is treated as "skip the namespace path" so M2 smoke
  tests are unaffected).
- `brokkr-sandbox/tests/mount_ns.rs` — three M3 evil-action tests:
  EV-01 (`cat /etc/shadow` fails inside the sandbox), `ls /` shows
  only the entries we put there, and EV-15 (host's
  `/proc/self/mountinfo` is byte-identical before and after the
  sandbox runs an explicit `mount -t tmpfs`).
- `nix` features extended to include `mount` and `sched`.
- M4: PID namespace + init reaper. `unshare` now also asks for
  `CLONE_NEWPID`; the runner forks twice — outer runner waits on init,
  init mounts `/proc` from inside the new pidns and forks the action,
  then loops on `waitpid(-1, …)` reaping orphans until the action
  exits. Both forks translate the action's `WaitStatus` back into the
  caller's exit code or signal so the host's
  `std::process::ExitStatus` mapping still works. `runner/pidns.rs` is
  the new module.
- `brokkr-sandbox/tests/pid_ns.rs` — three M4 evil-action tests:
  AC-01 (`/proc/1/comm` is `brokkr-sandboxd`), EV-16 (action's `$$`
  is 2 and `/proc` shows only single-digit PIDs), EV-13 (orphaned
  `sleep 60` does not outlive the sandbox — pidns teardown SIGKILLs
  it).
- `nix` features extended to include `signal` (for `raise` / signal
  re-delivery in the reaper).
- M5: network namespace + optional loopback. `unshare` now also asks
  for `CLONE_NEWNET`, so the action lands in an empty netns by
  default — no interfaces, no routes, not even loopback. With
  `NetworkPolicy::Loopback`, the runner sends a hand-rolled
  `RTM_NEWLINK` over a raw netlink socket to flip `lo` `UP` before
  the action exec. New `runner/netns.rs` module; ~150 lines, no
  extra crate dependencies.
- `brokkr-sandbox/tests/net_ns.rs` — three M5 tests using `python3`
  to read `errno` through the action's exit code: EV-08 (`1.1.1.1:443`
  is `ENETUNREACH`), policy-`None` makes `127.0.0.1` `ENETUNREACH`
  too (lo is `DOWN`), and policy-`Loopback` upgrades `127.0.0.1`'s
  failure mode from `ENETUNREACH` to `ECONNREFUSED` — proving the
  link is actually up.
- M6: per-action cgroup-v2 + wall-clock timeout + OOM detection +
  resource accounting. `Sandbox::with_cgroup_root(path)` opts into
  it; without that builder call the sandbox does no cgroup work and
  M2-M5 behaviour is preserved. New host module `host/cgroup.rs`
  creates `<root>/action-<uuid>/`, applies `memory.max` /
  `memory.swap.max` / `pids.max` / `cpu.max` from
  `ResourceLimits`, attaches the runner pid to `cgroup.procs`
  before the runner makes any progress, reads `cpu.stat` /
  `memory.peak` / `pids.peak` / `io.stat` for accounting, and
  checks `memory.events:oom_kill` to translate kernel-OOM kills into
  `ExitStatus::OutOfMemory`. `cgroup.kill` is used for atomic
  cleanup on wall-clock timeout (kernel ≥ 5.14, with a per-pid
  fallback).
- Host-side wall-clock enforcement: when `ResourceLimits.wall_clock_secs`
  is set, the runner wait is wrapped in `tokio::time::timeout`; on
  elapsed the cgroup is `kill_all`'d (or the runner alone, sans
  cgroup) and `ExitStatus::Timeout` is returned.
- `host/linux.rs` now drives stdout/stderr drains via
  `tokio::spawn(read_to_end)` rather than `wait_with_output`, so
  `child.wait()` is interruptible by the timeout path.
- `brokkr-sandbox/tests/cgroup.rs` — four M6 tests:
  `wall_clock_timeout_kills_long_action` (always runs),
  `fork_bomb_capped_by_pids_max`, `memory_max_triggers_oom_status`,
  `accounting_is_populated_for_a_normal_action`. The last three
  skip cleanly unless `BROKKR_TEST_CGROUP_ROOT` is set or
  `/sys/fs/cgroup/brokkr.slice/` is writable; locally they pass under
  `systemd-run --user --scope -- bash -c 'BROKKR_TEST_CGROUP_ROOT=…
  cargo test'`.
- Added `uuid` as a direct dep of `brokkr-sandbox` for action-cgroup
  naming.

## Phase 3 (in progress)

### Added
- `docs/phase-3-plan.md` — detailed Phase 3 implementation plan
  (failure model, rendezvous-hashing routing, tiered storage
  composition, replication strategy, FUSE materialisation,
  garbage-collection sketch, M0–M7 milestones, CI / host
  compatibility notes, deferred items).
- `docs/journal/phase-3.md` — Phase 3 journal, started with the M0
  entry. Records the architecture decisions made before any code
  lands (HRW over consistent hashing, push-based membership stream,
  `R/2+1` write quorum + async repair).
- M1: `brokkr.v1.membership.proto` — `CasNode`, `NodeStatus`,
  `TopologyView`, and `MembershipService.WatchTopology` (long-lived
  server-streaming RPC). The existing `brokkr.v1` module gains new
  types; the build script picks up the new proto.
- M1: `brokkr-cas::ring` — rendezvous-hash (HRW) replica picker.
  Pure-data `RingNode` + `NodeStatus`; `replicas_for(digest, nodes,
  R)` returns the top-R eligible nodes ordered primary-first.
  Backed by sha256. Seven unit tests, including distribution
  uniformity (10k digests over 4 nodes, ±10% per node) and
  one-node-removal churn (~20% in expectation, asserted in the
  10–35% band on a 4k-digest sample).
- M1: `brokkr-cas::router::Router` — client-side topology view
  built on top of `ring`. Atomically swappable via
  `update_topology`; `primary_replicas_for(&digest)` consults the
  current view at the configured replication factor. `RwLock`-based
  read path; lock poisoning recovery is graceful. Four unit tests.
- M1: `brokkr-control::membership::Membership` +
  `MembershipServiceImpl` — `Membership` holds a
  `watch::Sender<TopologyView>` that every subscribed client tails;
  `set_nodes` / `set_replication_factor` bump the generation iff
  the effective view changes (`send_if_modified`).
  `MembershipServiceImpl` adapts the watch receiver into a tonic
  server-stream. Five unit tests.
- M1: `brokkr-control/tests/membership.rs` — two integration tests.
  `watch_topology_streams_current_view_then_updates` exercises the
  gRPC flow end-to-end: first message on connect is the current
  view, subsequent messages arrive on `set_nodes`.
  `router_routes_consistently_with_local_topology` proves the
  proto round-trip is lossless: a `Router` fed from the gRPC
  stream picks the same primary for each digest as one built
  directly from a hand-rolled `Topology`.
- `tokio-stream` gains the `sync` feature on `brokkr-control` for
  `WatchStream`.
- M2: `brokkr-cas::bloom::Bloom` — hand-rolled bloom filter sized
  from `(expected_items, fp_rate)` via the standard formulas
  (`m = ⌈-n·ln p / (ln 2)²⌉`, `k = ⌈(m/n)·ln 2⌉`). Hashes are
  derived from the digest's existing sha256 via the
  Kirsch–Mitzenmacher construction (`h_i = h1 + i·h2` mod m), so
  insert/check is plain bit-ops without re-hashing. Seven unit
  tests including an empirical false-positive-rate check (10k
  members at p=0.01, 100k probes, rate < 2% asserted) and a
  sizing-formula spot check (1M @ 1% → ~9.6 Mbits, k=7).
- M2: `brokkr-cas::BloomCas<C: Cas>` — decorator wrapping any
  `Cas` backend. `find_missing_blobs` partitions inputs into
  bloom-says-missing vs. bloom-says-maybe; only the latter
  consults the underlying backend. `batch_update_blobs`
  delegates and inserts into the bloom on success.
  `batch_read_blobs` delegates unchanged (the bloom doesn't help
  reads). `rebuild_from` reseeds the filter from an authoritative
  digest source for the periodic-rebuild path. Six tests.
- No new direct deps — bloom is built from the existing
  `sha2` and standard library.
- M3a: `brokkr-cas::tiered::TieredCas<W: Cas>` — composes an
  in-memory size-bounded LRU "hot" tier in front of any `Cas`
  backend (typically `RedbCas`). Reads serve from hot on hit;
  hot misses fall through to warm and promote on success. Writes
  populate warm authoritatively and hot eagerly (workers
  frequently re-read what they just wrote). `find_missing_blobs`
  delegates straight to warm — hot is a cache, not authoritative.
- M3a: Hand-rolled `HotTier` LRU. Byte-bounded (not entry-bounded
  — blob sizes vary too widely for a count to be useful). Hash
  map for O(1) lookups + intrusive doubly-linked-list of `usize`
  indices into a node pool, with a free-list for evicted slots.
  All safe Rust. Blobs larger than the whole capacity are not
  cached (avoids evicting the entire tier for one outsized
  insert). Zero capacity disables hot caching entirely (useful
  for tests).
- M3a: Eleven unit tests on `HotTier` + `TieredCas` covering
  empty get, put/get round-trip, LRU eviction, MRU touch,
  oversized-blob skip, zero-capacity bypass, hot warmup on
  write, warm-to-hot promotion on read, find-missing delegation,
  and rejection of failed writes from the cache.
- Cold tier (OpenDAL S3) is deferred to M3b — a follow-up PR.
- M4: `brokkr-cas::replicated::ReplicatedCas<P: ReplicaPool>` —
  quorum-write + read-fan-out across the `R` replicas selected
  by the rendezvous ring. Writes succeed at `⌈R/2⌉ + 1` acks;
  reads try replicas primary-first and return the first
  success. `find_missing_blobs` queries the primary (with
  failover to the next replica if the primary is unreachable).
- M4: `ReplicaPool` trait + `StaticPool` impl. The pool maps
  `node_id → Arc<dyn Cas>`; a future milestone will provide a
  gRPC-backed pool, but M4 ships the logic and tests in-process
  against pools of `InMemoryCas` instances.
- M4: Seven `ReplicatedCas` tests — write fan-out lands on
  exactly R replicas, read serves from first-available, read
  returns NotFound when no replica has the blob, quorum holds
  on one-replica-down with R=3, quorum fails when only 1/2
  reachable, find-missing returns authoritative answer, empty
  topology fails closed.
- `futures` workspace dep added to `brokkr-cas` for
  `future::join_all` on the replica fan-out.
- M5a: `brokkr-cas::gc` — reference-counted GC for the CAS.
  `plan(&cas, &action_cache)` walks every cached `ActionResult`,
  extracts the digests inlined directly (output_files,
  stdout/stderr, output_directories' tree / root digests), and
  returns the set difference `local_digests − reachable` as the
  candidate-deletion list. `sweep` runs `plan` and deletes every
  unreachable digest; `sweep_with_plan` lets callers dry-run +
  apply a custom retention filter between the two steps.
- M5a: `Cas` trait gains `list_digests` and `delete_blob`
  default-implemented as empty / no-op so non-GC test backends
  still compile. `InMemoryCas` and `RedbCas` both override.
  `ActionCache` trait gains `list_entries` (same pattern);
  `RedbActionCache` overrides.
- M5a: six unit tests on `gc` — direct-digest extraction (output
  + stdout + stderr), malformed-proto skip, plan correctness on
  a 2-blob CAS with one cached output, sweep deletes the right
  set, empty action cache → everything unreachable, sweep
  idempotency.
- Transitive reachability (walking `Directory` Merkle DAGs) is
  deferred to a later milestone — that walk requires CAS reads
  and wants its own scheduling. M5a's non-transitive
  reachability covers output files (the bulk of blob volume).
- Retention window + atime tracking deferred to M5b; M5a deletes
  unreachable blobs immediately.
- M5b: `brokkr-cas::peer::repair_node(pool, topology, target)` —
  reconciles one target node's local digest set with what HRW
  says it should hold. Scans the universe of digests across all
  reachable replicas, computes HRW assignment for each, pulls
  bytes from peers for any the target is missing. Returns a
  `RepairReport` summarising `expected` / `already_present` /
  `repaired` / `unrepairable`. `repair_cluster` runs
  `repair_node` against every node.
- M5b: Five unit tests on `peer` — no-op when cluster is
  consistent, restore-a-lost-blob, target-doesn't-get-blobs-it-
  shouldn't-hold (HRW-aware), every-replica-lost-it edge case,
  and `repair_cluster` idempotency.
- M5b: Repair is built on top of M4's `ReplicaPool` abstraction;
  in-process tests use `StaticPool<InMemoryCas>`. A future
  milestone will wrap a gRPC pool (`CasPeer` clients) for
  cross-process repair. The daemon loop / scheduler is also
  deferred — `repair_node` is a one-shot primitive.
- M6a: `brokkr-cas::tree::materialize_tree(cas, root_digest,
  target_dir)` walks a REAPI Directory Merkle DAG and writes a
  faithful copy of the input tree to disk. Files are fetched
  from CAS lazily during the walk; symlinks become real
  symlinks; the Unix executable bit is honoured. Returns
  `MaterializationStats { files, dirs, symlinks, bytes }`.
- M6a: `build_tree_into(cas, source_dir)` — symmetric helper
  that packs a local directory tree into CAS (one `Directory`
  per actual directory, one blob per actual file) and returns
  the root digest. Used by the round-trip tests and useful for
  workers that want to upload their workspace.
- M6a: six unit tests cover empty trees, flat files, nested
  trees, executable-bit preservation, symlink preservation, and
  NotFound propagation on a bogus root digest.
- M6a: `CasError::Other(String)` variant for non-`Io`/non-`Redb`
  failures (proto decode, malformed tree entries). The tree
  module raises it on encode/decode errors.
- FUSE-based lazy materialisation deferred to M6b. M6a is the
  pre-FUSE foundation: workers can use it today on Phase 3
  clusters; M6b will replace it with a FUSE filesystem so trees
  bigger than RAM mount in ~ms without copying every byte.
