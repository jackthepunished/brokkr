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
