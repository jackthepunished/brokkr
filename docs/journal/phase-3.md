# Phase 3 — Distributed Cache

- **Status:** in progress
- **Plan:** `docs/phase-3-plan.md`
- **Started:** 2026-05-14

This journal accumulates short retrospectives as each milestone (M0–M7)
lands. Each milestone is a single PR; each section here is the post-merge
debrief.

## M0 — `feat/phase3-plan`

- **Date:** 2026-05-14
- **PR:** _(filled in after merge)_
- **Outcome:** Phase 3 has a buildable plan
  (`docs/phase-3-plan.md`). The plan picks rendezvous hashing
  over consistent hashing (Appendix A), commits to OpenDAL for the
  cold tier (M3), defers distributed-action-cache work to Phase 5,
  and slices the phase into seven shippable milestones plus this
  stub.

### Open questions resolved before M1

- **HRW vs. consistent hashing.** HRW. Brokkr's `N` is small in
  Phase 3 (single-digit CAS nodes); the simpler math and
  ~30-line implementation beat consistent-hashing's
  `O(log N)`-with-virtual-nodes complexity.
- **Membership update model.** Push (long-lived
  `WatchTopology` stream) rather than periodic polling. Lower
  latency on cluster events; the control plane doesn't take
  steady-state polling load.
- **Write quorum.** `R/2 + 1` majority + async peer repair.
  Strict-all-R would block on the slowest node; the cold-tier
  S3 write is the durability backstop for a fully-committed
  blob.

### Open questions deferred to specific milestones

- **OpenDAL S3 dep size.** Decision deferred to M3 (the
  cold-tier milestone). Likely: a Cargo feature gate so tests
  default to no-S3.
- **FUSE on WSL2 / macOS.** M6 will add a `--check-host` probe
  and skip cleanly when unsupported. Same pattern as the M3+
  unprivileged-userns skip macro.
- **Node identity persistence.** M1 will write a UUID to
  `<data_dir>/node_id` on first start.

### What surprised me

- _(filled in once M0 lands)_

## M1 — `feat/phase3-membership-and-ring`

- **Date:** 2026-05-14
- **PR:** _(filled in after merge)_
- **Outcome:** Plumbing layer of Phase 3. A
  `brokkr.v1.MembershipService.WatchTopology` RPC publishes the
  `TopologyView` as a long-lived server-streamed RPC.
  `brokkr-cas::ring` implements rendezvous (HRW) hashing;
  `brokkr-cas::router` wraps it with an atomically-swappable
  topology view. The control plane's `Membership` handle is the
  source of truth; mutations bump the generation iff the
  effective view changed (`watch::Sender::send_if_modified`).
  Two integration tests prove the gRPC round-trip — a `Router`
  fed from the stream picks the same replicas as one built from
  a hand-rolled `Topology`.

### Decisions

- **Decouple `ring` from `brokkr-proto`.** The HRW code takes
  pure `RingNode` / `NodeStatus` values, not the
  `brokkr.v1.CasNode` proto. Two reasons: (1) the routing logic
  compiles and tests in `brokkr-cas` without dragging in the
  proto-generated code, and (2) the proto's `i32`-encoded
  `NodeStatus` is a UX hazard if exposed to callers. The
  integration test does the proto → ring conversion explicitly.
- **`Membership::set_nodes` is idempotent.** `send_if_modified`
  on the watch channel skips the wake-up when the configuration
  hasn't changed. Clients use the generation as a cache key;
  re-emitting the same view (operator restart, config reload)
  would force redundant work everywhere.
- **`Suspect` nodes participate in writes.** They're eligible
  in `replicas_for`; the *caller* gets to prefer Healthy over
  Suspect on the read path. The distinction matters for the
  failure model: a brief network blip shouldn't cause data to
  skip a node and require peer-repair when the node recovers.
- **In-process integration test, not a real two-process
  cluster.** No CAS-node binary exists yet (that's M3-M4); M1
  only needs to prove the control plane → router pipeline.

### What surprised me

- **`tokio-stream` features are split smaller than I
  expected.** `WatchStream` is gated behind the `sync` feature
  on `tokio-stream` (not `tokio` itself). Easy to miss; same
  flavour as Phase 2's `nix` per-submodule features.
- **HRW distribution uniformity is *really* good.** I'd
  expected to need a ±15-20% band on the 10k/4-node test; ±10%
  is comfortable. The distribution test passes by a wide
  margin — the right kind of surprise.
- **`send_if_modified` is one line less than I almost wrote.**
  Spent a minute looking for `set_nodes_silent` before
  realising the tokio API already has the idempotent path.
  Reading module docs beats reaching for the obvious method
  name.

## M2 — `feat/phase3-bloom-filter`

- **Date:** 2026-05-14
- **PR:** _(filled in after merge)_
- **Outcome:** Hand-rolled bloom filter in `brokkr-cas::bloom`
  plus a `BloomCas<C: Cas>` decorator that any backend can wear
  to short-circuit `find_missing_blobs` on definitely-absent
  digests. ~280 lines total including 13 tests. No new
  third-party deps.

### Decisions

- **Hand-roll, don't pull a crate.** Considered
  `growable-bloom-filter` (1.4k stars, MIT) and a few smaller
  alternatives. The filter we need is genuinely 150 lines of
  arithmetic and `Vec<u64>` indexing; a dep would have been more
  code to audit than the filter itself.
- **Kirsch–Mitzenmacher hash derivation.** The textbook trick of
  generating `k` hashes from two independent base hashes
  (`h_i = h1 + i·h2 mod m`) is asymptotically equivalent to `k`
  truly independent hashes and avoids re-running sha256 per
  insert/check. We get `h1`/`h2` for free by parsing the leading
  32 hex chars of the digest's existing sha256.
- **Decorator over modification.** Instead of bolting the bloom
  into `RedbCas` directly, `BloomCas<C: Cas>` wraps any backend.
  Composes with `Arc<dyn Cas>` and keeps the bloom optional —
  Phase 1 paths that already use `RedbCas` unmodified stay
  untouched.
- **No bloom on the read path.** `batch_read_blobs` delegates
  unchanged. A bloom hit would save us one disk lookup vs. the
  backend's `NotFound`, but the backend already returns
  `NotFound` cheaply, and skipping the backend on a
  bloom-says-missing answer would risk returning a false
  negative if the bloom is stale (between an insert and the
  filter update, however briefly). The integrity rule "reads
  must always hit the backing store" wins.
- **Saturating insert counter, not exact cardinality.** The
  bloom can't subtract; duplicates inflate the counter. We
  expose `approximate_items` rather than `cardinality` to make
  this explicit.

### What surprised me

- **The false-positive-rate property test is generous and still
  passes by a wide margin.** Configured for 1%, the measured
  rate at 10k members + 100k probes is consistently
  ~0.4–0.7%. Probably the bound on the test
  (`< 2 * target_p`) could be tightened, but I'd rather absorb
  the variance than chase a flake later.
- **`div_ceil` on `usize` is stable.** I'd hand-rolled
  `(m + 63) / 64` and clippy suggested
  `usize::div_ceil(63 + m, 64)`. Either way is fine, but the
  named method is clearer.
- **`bits.len() * 64` overflowing on a 64-bit machine isn't
  realistic.** A `Vec<u64>` would have to be `u64::MAX / 64` ≈
  290 exabits long before this matters. Documented the lack of
  overflow check in the source.

## M3a — `feat/phase3-tiered-storage`

- **Date:** 2026-05-14
- **PR:** _(filled in after merge)_
- **Outcome:** `brokkr-cas::tiered::TieredCas<W: Cas>` composes
  an in-memory size-bounded LRU "hot" tier in front of any `Cas`
  backend. Hand-rolled `HotTier` with hash-map + doubly-linked-list
  via `usize` indices into a node pool, all safe Rust. Eleven
  unit tests. Cold (S3) tier deferred to M3b.

### Decisions

- **Decorator pattern again.** Mirrors `BloomCas` and is the
  same composability win: a test that wants undecorated
  `RedbCas` keeps having it; a node that wants hot caching wraps.
- **Byte-bounded LRU, not entry-bounded.** Blob sizes vary from
  hundreds of bytes (action protos) to multiple MiB (compiler
  binaries). An entry count can't size memory; a byte budget
  can. Single-blob inserts that exceed the whole budget are
  silently skipped — caching a 4 GiB blob in a 64 MiB hot tier
  would evict everything else for one promotion.
- **Eager write-through to hot.** Workers re-read their own
  outputs (action results inline blobs they just produced). The
  hot tier is the right place for those by definition.
- **`find_missing_blobs` skips hot entirely.** Hot is a cache,
  not authoritative; a blob can be evicted but still present in
  warm, so a hot-miss answer would be wrong. The bloom (M2)
  short-circuits on the absent side; hot only helps reads.
- **Split M3 into M3a + M3b.** OpenDAL pulls a large dep tree
  (rustls / hyper / xml / opendal-core); landing it together
  with the in-memory LRU would have made the PR's review
  surface much wider than it needs to be. Cold tier is its own
  PR with its own feature gate.

### What surprised me

- **Safe Rust + `usize` indices is the cleanest LRU shape.**
  I started with `Box<Node>` and prev/next `Option<NonNull<Node>>`
  before remembering this is a code smell — `Vec<Slot>` with
  `usize` prev/next and a free-list is shorter, has no
  unsafe, and reuses node slots on eviction. The free-list adds
  ten lines; reusing slots saves more than ten in lifetime hassle.
- **`Bytes::clone` being cheap is load-bearing here.** The
  promote-on-read path clones the warm response into the hot
  tier. If `Bytes` weren't Arc-backed, this would copy the
  whole blob per promotion. As it is, we move one `Arc`.
- **Hot-tier `Mutex` is fine; an `RwLock` would buy us less
  than expected.** LRU touch on `get` needs a write lock anyway
  (moving the entry to MRU). The only contention-relevant case
  is concurrent reads that *both* miss hot — and those serialize
  on the warm backend, not on the hot lock.
