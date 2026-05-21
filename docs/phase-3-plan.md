# Phase 3 — Distributed Cache: Implementation Plan

> **Scope.** This document elaborates `docs/plan.md` §15 ("Phase 3 —
> Distributed Cache") into a buildable, reviewable plan. Phase 3 turns
> Brokkr's single-node CAS into a horizontally-scalable, replicated,
> tiered storage system, and replaces the worker's eager input copy
> with on-demand FUSE materialization.
>
> **Audience.** Anyone (human or LLM) implementing Phase 3 work. Read
> `docs/plan.md` (architecture invariants), `docs/phase-2-plan.md`
> (Phase 2 retrospective context), and the Dynamo / Bigtable papers
> before starting.

---

## 1. Goal & Non-Goals

### 1.1 Goal

After Phase 3, Brokkr's data plane satisfies all of these
simultaneously:

1. **Horizontal scale.** N CAS nodes share the keyspace via stable
   hashing. Adding or removing a node moves at most `~K/N` blobs
   (where `K` is the total blob count), not every blob.
2. **Fault tolerance.** Every blob is replicated to `R` distinct
   nodes (default `R=2`). Any single node failure is invisible to
   reads.
3. **Tiered storage.** Each node has a hot in-memory LRU, a warm
   local-disk tier (the existing redb-backed store), and a cold tier
   on S3-compatible object storage via [OpenDAL]. Promotion / demotion
   between tiers is automatic.
4. **Lazy input materialization.** Worker actions see their input
   tree as a FUSE filesystem. Files are fetched from CAS only on
   first `read(2)`. A 5 GiB input tree should mount in < 100 ms;
   only files actually read transfer bytes.
5. **Garbage collection.** Blobs not referenced by any live
   ActionResult and not accessed within the retention window are
   evicted, freeing capacity without losing reachable data.
6. **Membership-aware routing.** A control-plane–published cluster
   topology lets every client / worker / CAS node route to the right
   replica without a centralized coordinator on the read path.

### 1.2 Non-Goals

These belong to later phases and must not creep into Phase 3:

- **Per-tenant ACLs / quotas** — Phase 4.
- **Erasure coding** (Reed-Solomon, Wirehair, etc.) — Phase 6+.
- **Cross-region replication / federation** — Phase 6+.
- **HA control plane** — Phase 5 (custom Raft).
- **Write-ahead replication consensus.** Phase 3 uses "write to all
  replicas, ack on R/2+1" without a per-blob Paxos round. The
  control-plane membership view is the source of truth for which
  nodes are alive.
- **Speculative read prefetch / read-ahead optimization** — measure
  first; optimize in Phase 6 if FUSE first-byte latency exceeds the
  target.
- **Online repair / anti-entropy.** Phase 3 ships a simple
  reconciliation script and counts on operators to run it; a
  continuously-running Merkle-tree anti-entropy daemon is Phase 6+.

---

## 2. Threat & Failure Model

Phase 3 is about **data plane availability** under partial failure,
not adversarial behaviour. The threat model from Phase 2 (untrusted
actions) is unchanged; this section enumerates the failure modes that
the distributed CAS has to survive.

| Failure | Acceptable outcome | Mitigation |
|---|---|---|
| Single CAS node crash | Reads succeed via surviving replica; writes complete on the survivors and the dead node is repaired on rejoin. | Replication factor R ≥ 2; quorum read = 1. |
| Network partition (one node isolated) | Isolated node refuses writes; reachable side keeps serving. | Membership view from the control plane is the source of truth. |
| Slow node (one replica several seconds behind) | Reads prefer the fastest healthy replica; writes treat slow as success once `R/2+1` ack. | Per-replica latency budget plus a write-quorum constant. |
| Cold-tier (S3) outage | Hot/warm tier serves cached blobs; reads of cold-only blobs fail with `Unavailable`. | OpenDAL retries with backoff; we surface the error rather than blocking forever. |
| Disk full on one node | That node refuses writes (returns `ResourceExhausted`); writes go to the other replicas only. | LRU eviction with low/high watermarks; surface to operator. |
| Bit rot on a stored blob | Reader detects via sha256 mismatch, refetches from another replica, repairs locally. | Checksum on every read. |
| Bloom-filter false positive | Worst case: an extra round-trip to a node that turns out not to have the blob. | Acceptable; bloom is a probabilistic fast path. |
| FUSE mount death | Worker re-mounts before next action; in-flight action fails fast with a structured error. | Auto-remount on `umount` failure; surface clear error. |

Phase 3 does **not** defend against:

- Permanent data loss when all `R` replicas of a blob are destroyed
  simultaneously. The retention window in S3 (cold tier) is the
  ultimate backstop; we don't try to be a backup system.
- Byzantine nodes (CAS nodes deliberately returning bad data). The
  cluster is trusted; we ship sha256 verification as defence in
  depth but assume good-faith failures.
- Clock skew across nodes. We carry timestamps for diagnostic
  purposes only; cluster correctness does not depend on synchronized
  clocks until Phase 5 Raft.

---

## 3. Architecture

### 3.1 Components

```
                ┌──────────────────────┐
                │   brokkr-control     │   publishes cluster topology
                │  (membership view)   │   (CasNode list, ring config)
                └──────────┬───────────┘
                           │ gRPC: GetTopology stream
                           ▼
   ┌───────────┐    ┌───────────┐    ┌───────────┐
   │  client   │    │  worker   │    │  CAS node │
   │ (rendezvous│    │ (rendezvous│    │ (peer talk│
   │  router)  │    │  router)  │    │  for repl)│
   └───────────┘    └───────────┘    └───────────┘
        │                │                  │
        ▼                ▼                  ▼
              ┌─────────────────────┐
              │ CAS NODE A          │
              │  hot LRU (in-mem)   │
              │  warm redb (disk)   │
              │  cold OpenDAL (S3)  │
              │  bloom filter       │
              └─────────────────────┘
              ┌─────────────────────┐
              │ CAS NODE B          │  ...
              └─────────────────────┘
```

**Three planes of communication:**

- **Topology stream** (`brokkr.v1.MembershipService.WatchTopology`):
  long-lived server-streamed RPC. The control plane is the source of
  truth for which CAS nodes exist and the ring configuration
  (rendezvous-hash secret, replication factor). Clients and workers
  reconnect on stream death.
- **REAPI CAS** (existing services): each CAS node implements
  `ContentAddressableStorage` and the `bytestream.ByteStream` service
  for blobs > 4 MiB. Clients pick the right node(s) using the
  topology view.
- **Peer replication** (`brokkr.v1.CasPeer`): a CAS-internal service
  for cross-node replication, repair, and bloom-filter exchange. Not
  exposed to clients.

### 3.2 Crate layout

Most of Phase 3 fits inside the existing `brokkr-cas` and
`brokkr-control` crates; the additions:

```
crates/brokkr-cas/
├── src/
│   ├── bloom.rs            # Bloom filter (M2)
│   ├── tiered/             # Hot / warm / cold tier composition (M3)
│   │   ├── mod.rs
│   │   ├── hot.rs          # In-memory LRU
│   │   ├── warm.rs         # Existing RedbCas, wrapped
│   │   └── cold.rs         # OpenDAL-backed
│   ├── ring.rs             # Rendezvous (HRW) hashing (M1)
│   ├── router.rs           # Client-side replica selection (M1)
│   ├── peer/               # Peer replication client + server (M4)
│   └── gc.rs               # Reference-count GC + LRU eviction (M5)
└── tests/
    ├── ring.rs             # Property tests on hash distribution
    ├── tiered.rs           # Tier promotion / demotion semantics
    ├── replication.rs      # Replica fanout, partial failure
    └── fuse.rs             # FUSE materialization (M6)

crates/brokkr-control/
├── src/
│   ├── membership.rs       # CasNode registry + topology stream (M1)
│   └── ...
└── ...

crates/brokkr-worker/
├── src/
│   ├── fuse.rs             # FUSE mount + lazy fetch (M6)
│   └── ...
```

New deps (all justified one-liners go in the relevant milestone PR):

- **`opendal`** for the cold tier (S3, MinIO, local-disk fallback).
- **`fuser`** for the FUSE filesystem.
- **`growable-bloom-filter`** for the bloom filter (or hand-rolled —
  decided in M2).
- **`lru`** for the hot tier (or hand-rolled with `std::collections`).

---

## 4. Public API additions

```rust
// brokkr-cas: existing Cas trait stays as-is. Adds a topology-aware
// composite:

pub struct ShardedCas {
    topology: TopologyView,
    local: Arc<dyn Cas>,             // this node's tiered store
    peers: PeerPool,                 // gRPC clients to other CAS nodes
}

#[async_trait]
impl Cas for ShardedCas {
    // FindMissingBlobs / BatchUpdateBlobs / BatchReadBlobs all consult
    // the topology, fan out to the responsible replicas, and merge.
}

pub trait TieredStore {
    async fn get(&self, digest: &Digest) -> Result<Option<Bytes>>;
    async fn put(&self, digest: &Digest, data: Bytes) -> Result<()>;
    async fn contains(&self, digest: &Digest) -> Result<bool>;
}

// Membership and topology:

#[derive(Debug, Clone)]
pub struct TopologyView {
    pub generation: u64,
    pub nodes: Vec<CasNode>,
    pub replication_factor: u32,
}

#[derive(Debug, Clone)]
pub struct CasNode {
    pub id: NodeId,
    pub endpoint: String,
    pub status: NodeStatus,
}

pub enum NodeStatus {
    Healthy,
    Suspect,
    Unreachable,
}
```

---

## 5. Subsystem designs

### 5.1 Rendezvous hashing (HRW)

**Why HRW, not consistent hashing.** HRW gives the same "single-node
churn" property as consistent hashing (adding/removing one node moves
`~K/N` blobs) but with simpler math (no virtual nodes) and a more
uniform load distribution under skewed workloads. It is also trivial
to implement: O(N) per lookup.

**Algorithm.**

```rust
fn replicas_for(digest: &Digest, nodes: &[CasNode], r: u32) -> Vec<&CasNode> {
    let mut scored: Vec<(_, _)> = nodes
        .iter()
        .filter(|n| n.status != NodeStatus::Unreachable)
        .map(|n| (weight_hash(&n.id, digest), n))
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0));
    scored.into_iter().take(r as usize).map(|(_, n)| n).collect()
}

fn weight_hash(node_id: &NodeId, digest: &Digest) -> u128 {
    let mut h = Sha256::new();
    h.update(node_id.as_bytes());
    h.update(digest.hash.as_bytes());
    let out = h.finalize();
    u128::from_be_bytes(out[..16].try_into().unwrap())
}
```

**Tests:** distribution uniformity (Kolmogorov-Smirnov against a
uniform), churn measurement on add/remove (≤ `K/N + slack`).

### 5.2 Bloom filter

Each node maintains a bloom filter over its held digests. The bloom
is consulted by `FindMissingBlobs` to short-circuit "definitely
missing" without a disk hit.

**Sizing.** Default target: 1M items, 1% false-positive rate → ~1.2
MiB per node. Bumpable per-node via config.

**Refresh.** The bloom is rebuilt on startup by scanning the warm
tier's redb table. After that, every successful `put` adds to the
bloom; deletes do not subtract (so the bloom is always a superset of
the actual contents, which is the safe direction). A periodic
rebuild (default: hourly) keeps the false-positive rate from drifting
upward.

**Peer-exchange (optional).** Phase 3 ships *without* gossip of
remote bloom filters; clients hit `FindMissingBlobs` on each
candidate replica. M2 lays the local filter; the peer-exchange
optimisation is a follow-up.

### 5.3 Tiered storage

```
hot       in-memory LRU            ~64 MiB           per process, sized via config
  ↓ miss                                            promote on hit
warm      redb on local disk      bounded by disk    existing RedbCas
  ↓ miss                                            promote on hit
cold      OpenDAL → S3/MinIO/local NVMe   ~unbounded promote on hit, demote on LRU
```

**Promotion policy.** Synchronous on the read path: a hit at warm
backfills hot; a hit at cold backfills warm and hot. Writes go to
hot + warm; cold receives a write only on a demotion (or via an
operator-triggered `--archive-stale` job).

**Demotion / eviction.** Hot is pure LRU with no demotion (data is
already in warm). Warm tracks atime per blob in redb; an LRU
eviction policy moves stale blobs to cold and removes them from
warm. Cold is "forever" — never auto-evicted; lifecycle policies
are the operator's responsibility on the S3 bucket.

**Atomic put across tiers.** Hot + warm puts use the same redb txn
for warm and an `Arc<Mutex<LruCache>>` for hot. Cold-tier writes are
async and best-effort (logged on failure); a missed cold write is
recoverable via reconciliation.

### 5.4 Replication

**Write path.** Client `BatchUpdateBlobs(blob)`:
1. Client computes the `R` responsible replicas via HRW.
2. Client sends `BatchUpdateBlobs` to all `R` replicas in parallel.
3. Client waits for `R/2 + 1` successes (or all failures). Return
   success on quorum; partial failures are surfaced as a structured
   error with which replicas succeeded.
4. Failed replicas are repaired asynchronously by the peer-replication
   service: each CAS node, on startup and periodically, asks every
   other node for blobs it's responsible for but doesn't have. (M5.)

**Read path.** Client `BatchReadBlobs(digest)`:
1. Client computes the `R` candidates.
2. Tries the first candidate (typically the lowest-RTT replica,
   tracked client-side).
3. On miss / error, tries the next.
4. Returns first success; on all-miss, the blob is genuinely absent.

**`FindMissingBlobs` fan-out.** A client asking which of N digests
are missing groups by responsible-replica, queries each replica for
its slice, merges results. A bloom-filter-aware path can later
short-circuit some of these queries.

### 5.5 FUSE input materialization

**Why FUSE.** A typical Bazel build produces an input tree with
thousands of files of which only hundreds are actually opened by
any one action. Materializing the entire tree to local disk is
wasteful. FUSE lets us serve `open(2)` / `read(2)` / `getattr(2)`
out of a userspace daemon that fetches from CAS lazily and caches
hot files locally for the lifetime of the action.

**Mount layout.**

```
/var/lib/brokkr/work/<job_id>/inputs/   # FUSE mount, owned by worker
  └── (whatever the action's input root looks like)
```

The mount is created per action and torn down after the action
exits.

**Lazy fetch implementation.**

- `getattr` / `lookup`: fetched from the directory tree (which is in
  CAS as a Merkle DAG; the worker already has it). No CAS fetch
  required.
- `open`: no fetch yet; just records the file's digest.
- `read`: on first read of an offset, fetches the entire file from
  CAS into a local tmpfile under
  `/var/lib/brokkr/work/<job_id>/cache/`. Subsequent reads serve out
  of the tmpfile. Files are kept memory-mapped for fast reads after
  the first.
- `release`: nothing — the cache persists for the action.

**Bandwidth ceiling.** FUSE serializes per-file open / read calls
from the kernel; a single action that touches K large files in
parallel limits to K-way concurrency. Acceptable for Phase 3; FUSE
optimisations are Phase 6+.

**Sandbox interaction.** The FUSE mount is on the host filesystem
under `/var/lib/brokkr/work/`; the sandbox `RootfsSpec.ro_binds`
includes this mount path under `/work/inputs/`. The sandbox's
mount-namespace setup needs no FUSE awareness — it's just a regular
filesystem from inside the sandbox.

**Failure modes.** If a CAS fetch fails inside `read`, FUSE returns
`-EIO` to the kernel and the action sees a read error. We surface
this in the worker log and in the ActionResult's `stderr_raw`.

#### 5.5.1 M6b — FUSE sub-plan

M6a (committed in 407f88a) gave us the eager-copy fallback
(`brokkr-cas::tree::materialize_tree`). M6b layers the FUSE
filesystem on top so multi-GiB input trees mount in ~ms and
only the bytes the action actually reads transfer from CAS.
This sub-plan freezes the design choices that §5.5 left open.

**Crate placement.** The FUSE filesystem lives in
`crates/brokkr-worker/src/fuse.rs`, **not** in `brokkr-cas`.
Rationale: `brokkr-cas` must stay portable (it compiles on
macOS / Windows for the CLI), `fuser` is Linux-only at
runtime, and the mount lifecycle is owned by the worker per
action. The filesystem depends on `brokkr-cas` for the
`Cas` trait and the `Directory` walk; no inverse dep.

**Public surface.**

```rust
// brokkr-worker/src/fuse.rs

/// A live FUSE mount for one action's input tree.
/// Dropping the handle unmounts and joins the background
/// fuser thread (with a bounded timeout, then `fusermount -uz`).
pub struct InputMount {
    mountpoint: PathBuf,
    cache_dir:  PathBuf,
    _bg: JoinHandle<()>,
    // ... unmount sentinel ...
}

pub struct InputMountSpec {
    pub root_digest: Digest,
    pub mountpoint:  PathBuf,    // /var/lib/brokkr/work/<job>/inputs
    pub cache_dir:   PathBuf,    // /var/lib/brokkr/work/<job>/cache
}

pub async fn mount(
    cas: Arc<dyn Cas>,
    spec: InputMountSpec,
) -> Result<InputMount, MountError>;
```

`mount()` is `async` because it pre-fetches the full
`Directory` Merkle DAG (small — only proto bytes, no file
content) before returning, so the kernel's first `readdir`
hits an in-memory tree. File content is fetched lazily.

**Inode table.** Walking the DAG builds a `Vec<Inode>` keyed
by `ino: u64` (starting at `FUSE_ROOT_ID = 1`). Each inode
carries:

```rust
enum InodeKind {
    Dir   { entries: HashMap<OsString, u64> /* name -> child ino */ },
    File  { digest: Digest, size: u64, exec: bool,
            cached: OnceCell<PathBuf> },
    Link  { target: OsString },
}
```

Symlink targets are returned verbatim from `readlink` (REAPI
v2 §SymlinkNode). The DAG is finite and acyclic by
construction (digests-as-IDs); no cycle detection.

**Lazy fetch on `read`.** First `read` for a file inode
fetches the whole blob from CAS into
`<cache_dir>/<hex(digest)>`, then `mmap`s it. `OnceCell`
serializes the fetch — concurrent kernel reads of the same
inode wait on the first fetcher rather than racing N
duplicate CAS round-trips. `read` then serves from the
mmap. We `fadvise(WILLNEED)` after mmap and
`fadvise(DONTNEED)` on `InputMount::drop` (§9 risk:
"memory-mapped FUSE backing files can survive umount").

**Tokio / fuser bridge.** `fuser::spawn_mount2` runs on a
dedicated OS thread (it loops on the `/dev/fuse` fd
synchronously). Lazy fetches inside FUSE callbacks need
async CAS calls — we hold a `tokio::runtime::Handle` and use
`handle.block_on(cas.get(&digest))` from the FUSE thread. A
shared `Arc<dyn Cas>` and a bounded `Semaphore` (default 16)
cap per-mount concurrent fetches so a runaway action can't
starve the worker's runtime.

**Mount lifecycle.** Owned by the worker, one mount per
running action. The job-runner sequence becomes:

```
1. resolve action.input_root_digest
2. let mount = fuse::mount(cas, spec).await?
3. update RootfsSpec.ro_binds with mount.mountpoint -> /work/inputs
4. run sandbox (existing Phase 2 path)
5. drop(mount)   // unmount + cleanup happens here
```

The drop guard uses `fuser`'s `BackgroundSession::join()` with
a 5 s timeout; on timeout we shell out to `fusermount -uz`
(lazy unmount) and log a warning. The job dir is rm-rf'd
afterwards.

**Sandbox interaction.** The mount path is a regular
filesystem from inside the sandbox — no FUSE awareness in
`brokkr-sandbox`. The bind is `MS_BIND | MS_REC | MS_RDONLY`,
same as any other ro input bind. The sandbox's mount
namespace is created after the FUSE mount is live, so the
FUSE fd belongs to the worker, not the sandbox.

**Host probe.** `brokkr-sandbox::checks::linux` gains a
`fuse_device` probe:

| Outcome | Trigger |
|---|---|
| `Pass` | `/dev/fuse` exists, readable, writable. |
| `Warn` | `/dev/fuse` exists but worker uid lacks rw (e.g. WSL default). Hint: `sudo chmod 666 /dev/fuse` or add user to `fuse` group. |
| `Fail` | `/dev/fuse` missing (no `fuse` kernel module). Hint: `sudo modprobe fuse` (WSL2) or kernel rebuild. |

This is surfaced through the existing `worker --check-host`
flag — no new flag.

**Failure shapes.** New `MountError` enum, `thiserror`:

```rust
#[derive(Debug, thiserror::Error)]
pub enum MountError {
    #[error("FUSE device unavailable: {0}")]
    Device(String),                       // -> /dev/fuse missing/eperm
    #[error("mount syscall failed: {0}")]
    Mount(std::io::Error),
    #[error("input tree walk failed: {0}")]
    Tree(#[from] brokkr_cas::CasError),
    #[error("mountpoint not empty: {0}")]
    Dirty(PathBuf),
}
```

`-EIO` is returned to the kernel for any in-flight `read`
failure, and the underlying `CasError` is logged with the
mount's `tracing` span (`fuse.mount=<path>, action=<id>`).

**Testing strategy.**

- **Unit tests, no FUSE.** The inode table builder
  (`Directory` proto → `Vec<Inode>`) is pure; tested in
  isolation against five fixtures (empty / flat / nested /
  exec-bit / symlink) shared with M6a.
- **Integration test, `#[cfg(target_os = "linux")]` +
  `#[ignore]` by default.** Mounts a synthetic 3-file tree
  against `InMemoryCas`, opens & reads each file, asserts
  byte equality, drops the handle, asserts the mountpoint is
  empty. Gated on `/dev/fuse` accessibility (skip with a
  log line otherwise — same shape as the existing sandbox
  tests).
- **Lazy-fetch assertion.** A wrapping `CountingCas` records
  every `get(digest)`; the test reads two of three files
  and asserts `get` was called exactly twice.
- **Soak hook for M7.** The three-node soak adds a 5 GiB
  synthetic tree and asserts mount time `< 100 ms` and
  measured CAS bytes transferred ≈ bytes-read-by-action.

**New deps.**

- `fuser = "0.15"` — actively maintained pure-Rust FUSE
  bindings, used by major projects (rust-fuse fork). License
  MIT. Optional `unprivileged` feature off by default; we
  use the privileged path.
- `memmap2 = "0.9"` — already in the tree? If not, justify
  as the obvious choice for `mmap` wrappers (BSD license,
  widely vendored).

**Out of scope for M6b** (explicit, to prevent creep):

- Output-tree FUSE (upload-on-`close`). Outputs stay
  materialised to local disk and walked by `build_tree_into`.
- macOS / FUSE-T support. `#[cfg(target_os = "linux")]`
  gates the entire module; non-Linux workers panic on
  `mount()` with a clear error. Phase 6 can revisit.
- Read-ahead / speculative prefetch (deferred per §1.2).
- Per-file LRU eviction of the cache. The cache lives for
  the lifetime of one action and is rm-rf'd on `Drop` — no
  intra-action eviction.

**Definition of done for M6b.**

1. `fuse::mount(spec)` returns a live `InputMount` in
   < 100 ms for a tree with up to 10k entries (DAG walk,
   no file fetches).
2. Reading a 1 MiB file through the mount produces exactly
   one CAS `get` for that digest; re-reading the same file
   produces zero further `get`s.
3. Dropping the `InputMount` unmounts cleanly within 5 s on
   the happy path; falls back to `fusermount -uz` and logs
   on timeout.
4. `worker --check-host` reports a `fuse_device` line and
   exits non-zero on `Fail`.
5. `cargo clippy --workspace --all-targets -- -D warnings`
   clean; new tests green on Linux; non-Linux compile
   succeeds (module is `cfg`-gated, with a stub returning
   `MountError::Device("not linux")`).
6. CHANGELOG `## Unreleased` entry; rustdoc on public
   surface; tracing span on `mount()` and per-`read`.

### 5.6 Garbage collection

**Reachability.** A blob is reachable if any of:

- It's the digest of an `Action`, `Command`, or `Directory` referenced
  by a non-expired `ActionResult` in the action cache.
- It's the digest of an output file referenced by a non-expired
  `ActionResult`.
- It's referenced (transitively) from a reachable `Directory` proto.

The control plane is the source of truth for action-cache contents,
so GC starts there: enumerate live `ActionResult`s, transitively
expand to all reachable digests, and publish that set to the CAS
nodes.

**Eviction.** Each CAS node, on receiving the reachability set:

1. Compute `unreachable = local_digests - reachable`.
2. For each unreachable digest, check its atime in the warm tier.
3. If atime is older than the retention window (default 30 days),
   delete from warm. Cold-tier eviction is operator policy.

**Frequency.** Runs once per day by default; tunable via
`--gc-interval` on the control plane.

**Safety.** GC never deletes a blob that's referenced from `live`
ActionResults; the retention window is an additional safety margin.
In Phase 3 we ship GC as a `brokk admin gc` CLI subcommand that
runs the algorithm once and exits, plus a control-plane daemon
loop. Both delegate to the same library code.

### 5.7 Routing

Clients (CLI + worker) embed a `Router`:

```rust
pub struct Router {
    topology: watch::Receiver<TopologyView>,
    clients: HashMap<NodeId, CasClient>,
}

impl Router {
    pub fn replicas_for(&self, digest: &Digest, r: u32) -> Vec<CasClient>;
    pub fn fan_out_read(&self, digest: &Digest) -> impl Stream<Result<Bytes>>;
    pub async fn fan_out_write(&self, blob: Blob, r: u32) -> Result<()>;
}
```

The router watches the topology stream and rebuilds its replica
maps on generation bumps. Clients dropping their topology
connection re-resolve.

---

## 6. Wire protocol additions

**Brokkr-internal protos** (`brokkr.v1`):

```protobuf
service MembershipService {
  // Long-lived stream: every topology change is pushed.
  rpc WatchTopology(WatchTopologyRequest) returns (stream TopologyView);
}

message TopologyView {
  uint64 generation = 1;
  repeated CasNode nodes = 2;
  uint32 replication_factor = 3;
  // Reserved for Phase 6+ ring secret rotation.
  bytes ring_secret = 4;
}

message CasNode {
  string node_id = 1;
  string endpoint = 2;
  NodeStatus status = 3;
  uint64 capacity_bytes = 4;
  uint64 used_bytes = 5;
}

enum NodeStatus {
  HEALTHY = 0;
  SUSPECT = 1;
  UNREACHABLE = 2;
}

service CasPeer {
  // Asynchronous replication: caller streams blobs the callee is
  // responsible for but doesn't have.
  rpc Replicate(stream PeerBlob) returns (ReplicateAck);
  // Bloom-filter exchange (optional in Phase 3 M2; M4 may add).
  rpc ExchangeBloom(BloomRequest) returns (BloomResponse);
}
```

REAPI protos are unchanged — `ContentAddressableStorage` and
`bytestream.ByteStream` already cover the client-facing CAS.

---

## 7. Testing strategy

### 7.1 Property tests

- **HRW distribution uniformity.** Generate 10k random digests
  against 10 nodes; assert each node gets ~`1k ± slack` blobs.
- **HRW churn.** Add/remove one node from a 10-node ring; ≤ `10%`
  of blobs change their primary replica.
- **Bloom false-positive rate.** Insert 100k digests into a
  filter sized for 100k @ 1%; query 1M random non-members; assert
  ≤ 2% false-positive rate.

### 7.2 Integration tests

- **Three-node cluster boot.** `brokkr-control` plus three
  `brokkr-cas` nodes via the existing in-process test harness;
  `brokk run -- echo hi` round-trips.
- **Single-node loss.** Same cluster; kill one CAS node mid-build;
  build continues; restart the dead node and observe peer-repair
  catch up.
- **Tier promotion.** Read a blob from the cold tier; observe it
  arrive in hot + warm after the call.
- **FUSE mount.** Action declares an input tree; FUSE mount
  succeeds in < 100 ms regardless of input-tree size; only files
  actually opened by the action transfer bytes.

### 7.3 Soak

- 1M random blob operations across a 3-node cluster, with one node
  restarting every 30 seconds. No data loss, no orphaned blobs.

#### 7.3.1 M7 — soak sub-plan

The soak runs as a `#[ignore]`-gated integration test under
`crates/brokkr-cas/tests/three_node_soak.rs`. Process model is
**in-process** — `R=2` `ReplicatedCas` over a `StaticPool` of
three `InMemoryCas` (or `RedbCas` with `tempfile::tempdir()` —
chosen below) nodes plus the existing `repair_node` primitive.
No second binary, no Docker, no localhost gRPC — those belong to
Phase 4's conformance suite. This soak's job is to stress the
distributed CAS *semantics* (write quorum, read fan-out, peer
repair after node loss) under continuous churn for a defined
budget.

**Backend choice.** `InMemoryCas`. The soak measures consistency,
not durability — wiping a node's state via
`mem::replace(&mut node, InMemoryCas::new())` is exactly the
"crash + cold restart" the plan calls out. Disk-backed `RedbCas`
would add filesystem variance without changing what we're
checking; defer to a Phase 4 conformance pass with `RedbCas`
when there's a real cluster binary.

**Default budget.** 25k operations and one restart every 250 ops
(≈ once per 2 s at the expected throughput) — small enough to
finish in under a minute on `cargo test --ignored` so devs can
run it locally, large enough to give peer-repair multiple churn
cycles to converge. The plan's 1M-op / 1-hour scenario stays
available via env vars (`BROKKR_SOAK_OPS`, `BROKKR_SOAK_CHURN`,
`BROKKR_SOAK_DURATION_S`) so CI can scale up; the test prints
the effective values at start so a failed run's log self-documents.

**Operation mix.** Three operations sampled with replacement:

| Op | Weight | Effect |
|---|---|---|
| `put` | 0.45 | New random 64–1024 B blob into `ReplicatedCas`; expect quorum success. Track in a "live" `HashSet<Digest>`. |
| `get` | 0.45 | Pick a random live digest, read via `ReplicatedCas`; expect success and byte equality. |
| `find_missing` | 0.10 | Mixed bag of live + non-live digests; expect exactly the non-live set back. |

A small seeded `StdRng` controls reproducibility; the seed is
printed at test start.

**Churn loop.** A background task picks a random non-primary
replica every `BROKKR_SOAK_CHURN` ops, swaps it with a fresh
empty `InMemoryCas` (simulating a cold restart), and then waits
for `repair_node` against that node to converge. Only one node
at a time is in the "restarting" state — invariant: the cluster
always has ≥ `R-1` healthy replicas of every blob (matching the
plan's tolerance of "one node down at a time").

**Invariants checked at the end** (and continuously where
cheap):

1. **No data loss.** Every digest in the live set reads
   successfully via `ReplicatedCas::batch_read_blobs` and matches
   the original bytes.
2. **No orphans.** `repair_cluster` after the soak ends reports
   zero `unrepairable` blobs and zero new repairs (idempotent).
3. **Peer-repair quiescence.** After the last operation, a final
   `repair_cluster` pass takes < 1 s — proves the cluster has
   already self-healed during the soak.
4. **Bounded blob count per node.** Every node's `list_digests`
   length matches the digests responsible for it under HRW
   (`replicas_for(digest, R)`) — no "phantom" entries from
   replay or double-writes.

**Why this isn't the soak the plan §11 calls out (yet).** The
plan promises a "1M-op, 1-hour, rolling-restart" run for the
phase's definition of done. That stays available via the env
vars (`BROKKR_SOAK_OPS=1000000`,
`BROKKR_SOAK_DURATION_S=3600`) and is intended for the release
gate in CI. The committed test uses the default 25k-op budget
so `cargo test -- --ignored` is a viable pre-merge check.

**Out of scope for M7.**

- A real two-binary cluster bootstrapped by an init script —
  there's no `brokkr-cas` server binary yet (the CAS is library
  code consumed by control). That's Phase 4 conformance work.
- Partial-network-partition simulation (jepsen-style) —
  explicitly out per §7.4.
- Adversarial test where the soak triggers a corrupted-byte
  return — the M3 plan calls out bit-rot detection on read
  (digest verify) but auto-repair is deferred to Phase 6 per
  §9 "Deferred".

**Files this milestone touches.**

- `crates/brokkr-cas/tests/three_node_soak.rs` — new, the soak
  itself (~200 lines).
- `docs/phase-3-plan.md` — this sub-plan (§7.3.1) and §11
  definition-of-done item 5 cross-reference.
- `docs/journal/phase-3.md` — M7 retrospective.
- `CHANGELOG.md` — Phase 3 Unreleased.

New dev-deps: `rand = "0.8"` is already in the lockfile
transitively; gate behind `[dev-dependencies]`.

### 7.4 What we do NOT test in Phase 3

- Jepsen-style consensus violations under arbitrary partitions —
  the action cache is the only strongly-consistent component, and
  it still lives on the single control-plane node in Phase 3.
  Distributed consensus is Phase 5.

---

## 8. Milestones (incremental delivery)

| #  | Branch                                       | Outcome                                                                                            | LOC est. |
|----|----------------------------------------------|----------------------------------------------------------------------------------------------------|----------|
| M0 | `feat/phase3-plan`                           | This plan doc plus the journal stub.                                                               | ~600     |
| M1 | `feat/phase3-membership-and-ring`            | `MembershipService` + topology stream; HRW hashing + `Router`; in-process test of fanned-out reads with one CAS node. | ~700 |
| M2 | `feat/phase3-bloom-filter`                   | Per-node bloom filter; `FindMissingBlobs` consults it before the disk hit; property-tested.        | ~400     |
| M3 | `feat/phase3-tiered-storage`                 | Hot / warm / cold tiers composed via `TieredStore`; OpenDAL behind a feature flag for the cold tier; tier-promotion tests. | ~800 |
| M4 | `feat/phase3-replication`                    | Quorum write + read fan-out across replicas; partial-failure tests; the existing single-node CAS becomes a `R=1` special case. | ~700 |
| M5 | `feat/phase3-peer-repair-and-gc`             | `CasPeer.Replicate` for async catch-up; `brokk admin gc` subcommand + control-plane GC daemon.       | ~700     |
| M6a | `feat/phase3-tree-materialization`          | Eager `materialize_tree` / `build_tree_into` in `brokkr-cas`: REAPI `Directory` ↔ on-disk tree, files/dirs/symlinks/exec-bit, six unit tests. Foundation for M6b. | ~450 |
| M6b | `feat/phase3-fuse-input-materialization`    | FUSE input mount in the worker via `fuser`; lazy fetch on first `read(2)`; per-action mount lifecycle; `--check-host` FUSE probe; integration test.            | ~750 |
| M7  | `feat/phase3-three-node-soak-and-journal`   | Three-node soak test (M3 §7.3); Phase 3 journal retrospective; plan updates.                       | ~250     |

Total: ~4.4k lines including tests.

Each milestone is a single PR that compiles, tests, and ships
independently. No milestone leaves the tree in a half-built state.

---

## 9. Risks & open questions

| Risk | Mitigation |
|---|---|
| OpenDAL's S3 backend pulls a large dep tree (rustls, hyper, etc.). | Gate behind a Cargo feature `cold-s3`; default-on for the binary, default-off for `cargo test --workspace` to keep test compile times reasonable. |
| FUSE on WSL2 is gated on `kernel.fuse`. | Surface in `--check-host` (Phase 3 M6 patch); skip FUSE tests gracefully when unsupported. |
| Membership stream re-connect storms after a control-plane restart. | Exponential backoff with jitter on the client; topology stream is read-only so re-connect is cheap. |
| Bloom-filter rebuild blocks reads. | Snapshot the warm tier with a redb read txn, build the new filter in a background task, atomically swap when ready. |
| HRW + node weights (heterogeneous capacity) interaction. | Phase 3 treats all nodes equal; Phase 4 may introduce weights once tenancy is in place. |
| Peer-replication can amplify on cold-start. | Throttle peer pulls to N concurrent fetches; surface as `Suspect` if a node falls more than `M` blobs behind. |
| Memory-mapped FUSE backing files can survive umount on Linux. | Explicit `fadvise(DONTNEED)` plus a worker shutdown hook that unlinks the cache before umount. |

### Open questions to resolve before M1

1. **Membership update model.** Push (stream) vs. periodic pull. Going
   with push for low latency; the alternative — clients poll every
   N seconds — adds steady-state load to the control plane.
2. **Node identity persistence.** Should `NodeId` be a stable UUID
   stored on disk, or derived from `(hostname, listen_addr)`? Phase
   3 picks "UUID written to `<data_dir>/node_id` at first start" so
   IP changes don't reshuffle the ring.
3. **Write quorum default.** `R/2 + 1` (strict majority) vs. all-R
   (more durable but blocks on any slow node). Going with majority
   plus async repair; durability for fully-committed blobs comes
   from S3 once a write makes it into cold.

### Deferred

- **Cold-tier compression.** Zstd-compressed blobs in S3 to reduce
  storage cost — Phase 6.
- **Read repair on bit-rot.** When a sha256 mismatch is detected,
  fetch from another replica and overwrite locally. Phase 3 ships
  detection (mismatch returns `DataLoss`); the auto-repair is
  Phase 6.
- **Bazel CAS conformance tests.** REAPI's conformance suite
  exercises edge cases (empty digest, mixed batch sizes); run it
  as part of Phase 4's Bazel-compatibility milestone.

---

## 10. CI & host compatibility

### 10.1 CI matrix additions

- The S3 tests need a MinIO container. GitHub Actions has a
  built-in `minio/minio` service image; we run it on the
  `ubuntu-latest` matrix entries only.
- FUSE tests need `/dev/fuse`. Modern GitHub runners have it on
  Ubuntu 22.04+. WSL2 needs the kernel's FUSE module enabled.
- Three-node soak test (M7) runs on `ubuntu-latest` only and is
  gated as `--ignored` by default (run once per release).

### 10.2 Local-dev compatibility

| Host | Status |
|---|---|
| Ubuntu 22.04+ | full support |
| Debian 12+ | full support |
| Fedora 38+ | full support |
| WSL2 (kernel ≥ 5.15) | full support; FUSE may need explicit `modprobe fuse` |
| macOS | partial — control plane + CLI compile, FUSE mount is FUSE-T or skipped |
| Windows | not supported |

---

## 11. Definition of done

Phase 3 is done when, on a clean Ubuntu 24.04 host with MinIO running:

1. A 3-node CAS cluster boots in under 30 seconds.
2. Killing any single CAS node does not interrupt an in-flight
   build; the surviving replicas serve every read.
3. A 5 GiB input tree mounts via FUSE in under 100 ms; only files
   actually read transfer bytes from CAS.
4. `brokk admin gc` evicts blobs unreferenced by the action cache
   and older than the retention window.
5. The M7 soak test (1M ops with one-node-rolling-restart) runs
   for 1 hour with no data loss.
6. `cargo clippy --workspace --all-targets -- -D warnings` is
   clean; `cargo test --workspace` is green; the M7 soak runs
   clean under `--ignored`.
7. The end-to-end Phase 1 + Phase 2 tests still pass — Phase 3 is
   a strict superset.
8. `docs/journal/phase-3.md` retrospective is written.

---

## 12. Out of scope (deferred)

- **Distributed action cache.** The action cache stays on the
  single control-plane node in Phase 3 (it becomes Raft-backed in
  Phase 5). The CAS becomes distributed; the metadata store does
  not.
- **Erasure coding** for cold-tier durability — Phase 6+.
- **Cross-region replication** — Phase 6+.
- **gRPC CAS protocol versioning / breaking changes.** Phase 3
  keeps REAPI v2 wire compatibility; brokkr.v1 protos are
  additive only.
- **Per-tenant quotas / accounting.** Phase 4.

---

## Appendix A — Why HRW over consistent hashing

| Property | Consistent hashing | Rendezvous (HRW) |
|---|---|---|
| Algorithm complexity | Ring + virtual nodes (typically 100–200 per node) | Score each node, take top R |
| Lookup cost | O(log N) with sorted virtual ring | O(N) |
| Memory cost | O(N × V) for V virtual nodes | O(N) |
| Load balance under skew | Good with high V, bad with low V | Excellent, no tuning |
| Add/remove churn | ~`K/N` blobs move | ~`K/N` blobs move |
| Replica selection | Pick R successors on the ring | Pick R highest-scoring |
| Implementation lines | ~150 + a sorted structure | ~30 |

Brokkr's N is small (single-digit nodes in Phase 3, two-digit in
Phase 4). The O(N) lookup is irrelevant at that scale; the simpler
math and smaller implementation are decisive.

---

## Appendix B — A worked example

What `brokk run -- gcc hello.c -o hello` looks like on a Phase-3
worker:

```
[trace] client::execute action_digest=…/254 cache_hit=miss
[trace] client::router replicas_for(action) = [cas-a, cas-b]
[debug] client::router uploaded Action to cas-a, cas-b in parallel
[trace] control::dispatch action_digest=…/254 job_id=…
[info ] worker received job; mounting inputs at /work/inputs (FUSE)
[debug] fuse mount completed in 18 ms (5 entries in input root)
[debug] sandbox::host spawned brokkr-sandboxd pid=… (M2 path; rootfs binds /work/inputs)
[debug] action opens hello.c → fuse fetch from cas-a (124 B)
[debug] action exec gcc → fuse fetches libc, ld, gcc-N.M from cas-a (12 MiB cold-cache hit, 96 ms first byte)
[debug] action exit 0; outputs declared: hello (16 KiB)
[trace] worker uploads hello to cas-a, cas-b (16 KiB each, quorum 2/2 acked in 8 ms)
[debug] fuse unmounted; worker tears down job dir
[trace] control::dispatch stored ActionResult; cache_hit=miss complete
```

The Phase 3 view: every CAS interaction goes through the topology
view; no single node is on the critical path for both blobs.
