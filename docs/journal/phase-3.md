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
