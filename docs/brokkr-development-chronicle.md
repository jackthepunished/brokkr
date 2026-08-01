# Building Brokkr — A Development Chronicle

*"Many hammers. One forge."*

A self-hosted Remote-Execution-API grid, built solo and in the open with Claude Code as a
standing pair, one gated phase at a time. This is the story of how it got from an empty
Cargo workspace to a fault-tolerant, multi-tenant compute cluster — told through the
project's own planning docs, ADRs, and phase journals.

- **Started:** 2026-04-28
- **Language:** Rust, edition 2021, MSRV 1.94
- **Crates:** 10
- **Current phase:** 5 complete → 6 (not started)
- **License:** Apache-2.0

| Phase | Theme | Status |
|---|---|---|
| 0 | Bootstrap | done |
| 1 | First End-to-End Slice | done |
| 2 | Hermetic Sandboxing | done |
| 3 | Distributed Cache | done |
| 4 | Scheduler & Multi-Tenancy | wrap-up (one tracked gap) |
| 5 | Consensus & HA (custom Raft) | done |

---

## Why it exists

Brokkr turns a fleet of Linux machines into one coherent grid for builds, tests, and
batch compute — speaking Bazel's Remote Execution API (REAPI) so existing tooling can
point at it without a rewrite. It exists for two stated reasons: CI is slow and
expensive everywhere, and building a REAPI server, a content-addressed cache, a
sandbox, a scheduler, and eventually a Raft implementation is, in the founder's own
words, "the most educational distributed-systems project I can attempt."

The project's axioms were fixed before a line of code existed, and they still show up
in every phase journal: determinism is sacred, correctness beats performance beats
convenience, and every sandbox layer assumes the layer above it is hostile. The
codebase is plain — `WorkerId`, `JobId`, `Digest` as newtypes, `bytes::Bytes` for
blobs, `thiserror` enums everywhere — but the roadmap is ambitious: hermetic
sandboxing on raw Linux primitives (no Docker, ever), a hand-rolled bloom filter and
rendezvous-hash ring for the cache, and a from-scratch Raft implementation waiting in
Phase 5.

---

## The operating model

What makes Brokkr's development process worth writing up isn't any single technical
choice — it's the discipline wrapped around them. Two documents anchor everything:
`docs/plan.md`, a ~1,200-line source of truth covering architecture, the six-phase
roadmap, and exit criteria per phase; and `CLAUDE.md`, a condensed operating manual
read by the AI on every session. Ten hard rules in that file — no `unwrap()` in
library code, no `unsafe` without a `// SAFETY:` comment, no Docker in the sandbox
crate, no imported Raft crate — are treated as non-negotiable, and the journals show
them being enforced against real temptation, not just stated as policy.

Each phase is scoped into small milestones (Phase 2's M1–M9, Phase 4's I1–I19), and
the convention is strict: **one milestone, one PR, one journal entry.** The entry
records what shipped, the decisions taken and why, and a "what surprised me" section —
which turns out to be where the most valuable engineering knowledge in the whole
project lives. Phases don't end until an exit-criteria review is written up honestly,
gaps and all — Phase 4's wrap-up flags a missed Bazel-compatibility demonstration
rather than quietly dropping it.

> **From CLAUDE.md:** *"If the user proposes an architectural change that conflicts
> with `docs/plan.md`, flag the conflict and ask whether to update the plan."*
>
> Phase 4 alone shows this rule firing repeatedly — dispatch model (ADR 0008), lease
> design (ADR 0009), tenancy (ADR 0010), and auth (ADR 0011) were all stopped-and-asked
> before implementation, not decided unilaterally mid-flight.

---

## Phase 0 — Bootstrap
*2026-04-28 · 9 crates · MSRV 1.78 → 1.85 · ADR 0001*

The goal was deliberately unglamorous: a Cargo workspace that builds, a `brokk
version` binary, and green CI on Linux x86_64 and aarch64. Nine crates were laid out
as a strict dependency DAG with `brokkr-common` as the only universal dependency — a
shape that held for the rest of the project without a single exception needed.

Two early surprises set the tone for how the project would handle friction: the MSRV
had to jump from 1.78 to 1.85 mid-bootstrap because a transitive dependency of Tonic
required edition 2024, and the vendored REAPI protobufs needed their module hierarchy
mirrored exactly (`build::bazel::remote::execution::v2`) because Prost's generated
`super::super::…` paths don't tolerate a flattened module tree. Both fixes are one
paragraph in the journal; neither derailed the phase.

---

## Phase 1 — First End-to-End Slice

Phase 1 threaded the thinnest possible path through every architectural layer: an
in-memory then on-disk CAS, a redb-backed action cache, a single-worker scheduler, and
a `brokk run -- echo hello` command that actually round-trips through gRPC and hits
the action cache on the second call. Nothing here is fast or distributed yet — the
worker runs jobs as a bare `tokio::process::Command`, exactly as `CLAUDE.md`'s
phase-awareness rule says it should at this stage — but the wire protocol, the digest
verification, and the tracing spans on the hot path (`client::execute` →
`control::dispatch` → `worker::run_action`) are the real thing, not stubs.

---

## Phase 2 — Hermetic Sandboxing
*M1–M9 · 2026-04-30 → 2026-05-14*

This is where Brokkr stops being a REAPI toy and starts being a systems project. The
goal: a worker executes actions inside a real Linux sandbox, built layer by layer with
no Docker, runc, or containerd anywhere near it — a hard rule, not a preference. Each
of the nine milestones added one isolation primitive, and each one earned at least one
genuinely hard-won lesson.

| M# | Layer added | What it proved |
|---|---|---|
| M1 | Host compatibility probes | Cross-distro kernel checks (WSL2's version string parses differently from stock Linux) |
| M2 | Re-exec runner skeleton | Phase-1 parity inside a new process model, via a JSON config pipe on fd 3 |
| M3 | Mount namespace + pivot_root | Sandboxed process cannot see `/etc/shadow` or the host mount tree |
| M4 | PID namespace + init reaper | Sandboxed process is PID 1; orphans are reaped, not leaked |
| M5 | Network namespace | No network by default, opt-in loopback only, proven via raw errno |
| M6 | cgroups v2 | Memory/CPU/PID limits; OOM and fork bombs are contained, not merely logged |
| M7 | seccomp-bpf + capability drop | Default-deny syscall filter; five new "evil action" tests pass |
| M8 | Determinism policy | Fixed hostname, forced UTC, scrubbed `LD_PRELOAD` — byte-identical reruns |
| M9 | Worker integration | Sandbox is the worker's default runner; `--no-sandbox` is the escape hatch |

> **Field note · M2:** `pipe2(O_CLOEXEC)` turned out to be load-bearing. Without it,
> the runner inherited a copy of its own pipe write-end through `fork`, so
> `read_to_end(fd 3)` on the host never saw EOF and the whole thing deadlocked.
> Roughly fourteen minutes were spent staring at a hung `brokkr-sandboxd` before the
> missing flag was found.

> **Field note · M7:** Capability dropping has exactly one legal order: Effective,
> then Permitted, then Inheritable. `capset(2)` revalidates the whole vector on every
> call and rejects any state where the new Effective isn't a subset of the new
> Permitted — dropping Inheritable first (the "obvious" order) fails immediately with
> an unhelpful `EPERM`. The seccomp mismatch action was also deliberately set to
> `EPERM` rather than a killing `SIGSYS`, on the theory that a debuggable errno trail
> is worth more to a build sandbox than a marginally stronger instant-kill guarantee.

By M9, every "evil action" test in the matrix passes: reading host secrets, mounting
filesystems, fork-bombing, allocating past the memory cap, resolving DNS, and reading
the CPU timestamp counter are all denied at the layer built to deny them.

---

## Phase 3 — Distributed Cache
*M0–M7 · 2026-05-14 → 2026-05-17*

Phase 3 took the single-node CAS from Phase 1 and made it a cluster: rendezvous (HRW)
hashing for replica placement, quorum writes with async peer repair, a hand-rolled
bloom filter to short-circuit `FindMissingBlobs`, a byte-bounded LRU hot tier, and —
the centerpiece — a FUSE filesystem that lazily materializes multi-gigabyte input
trees without copying bytes the action never reads.

The recurring architectural move across this phase is the **decorator pattern**:
`BloomCas<C>`, `TieredCas<W>`, and `ReplicatedCas<P>` all wrap any `Cas`
implementation rather than modifying one directly, so a test that wants a bare
`RedbCas` keeps having exactly that, and every layer composes independently. It's
also, by the project's own count, one of the cheaper phases dependency-wise — the
bloom filter and the LRU are both hand-rolled rather than imported, on the reasoning
that each was genuinely a couple hundred lines of arithmetic, cheaper to write than to
audit a crate for.

> **Field note · M6b:** Unmounting a FUSE filesystem correctly took three attempts to
> get right. `BackgroundSession::join()` alone hangs forever, because fuser's
> background loop only exits once its `/dev/fuse` descriptor is closed by the unmount
> — which is exactly the step that `join()` is waiting to happen. The fix,
> `umount_and_join()`, explicitly unmounts before joining, wrapped in a 5-second
> timeout with a `fusermount -uz` lazy-detach fallback for when the kernel is still
> mid-`read`.

**Phase 3 in numbers:** 8 milestones shipped (M6 split into a/b) · ~4k lines of new
code + tests · zero existing tests broken across Phases 1–2 · 3 new external deps
(`fuser`, `memmap2`, `rand` dev-only).

The phase closed with a three-node soak test churning one node in and out every `N`
operations, asserting zero data loss and sub-second reconvergence — with the
OpenDAL-backed cold storage tier and the `brokk admin gc` CLI explicitly named and
deferred to Phase 4, rather than silently dropped.

---

## Phase 4 — Scheduler & Multi-Tenancy
*I1–I19 · PRs #98–#116 · started 2026-06-27*

Phase 4 is where Brokkr stopped being a single-worker demo and became a cluster that
can lose a machine mid-job and keep going. It's also the phase with the clearest
example of the project's incremental discipline: nineteen numbered increments, each
landing a thin, fully-tested slice, several of them explicitly split into a
"foundation" PR (pure data structures, unit-tested in isolation) followed by a
"wiring" PR (the riskier integration).

**The chain, increment by increment:**

- **I1–I4** — a worker registry with injected-clock heartbeat eviction, closing with
  an end-to-end liveness loop.
- **I5–I7** — REAPI platform-constraint matching, wired into the scheduler as
  admission control, then into the worker as capability advertisement.
- **I8–I10** — the multi-worker dispatch redesign (ADR 0008): per-worker queues with
  submit-time routing, plus `SimpleFifo` and `BinPacking` strategies.
- **I11–I14** — job leases with crash reassignment (ADR 0009): a global pending
  queue, an expiry reaper, and — the cleanest fix of the whole phase — tying lease
  renewal to the existing heartbeat, so a lease only expires when a worker is
  actually dead.
- **I15–I17** — tenants and fair scheduling (ADR 0010): a Start-time Fair Queue with
  per-tenant virtual clocks, then per-tenant concurrency quotas.
- **I18–I19** — client authentication (ADR 0011): JWT bearer tokens with an
  authoritative tenant claim, plus worker↔control mTLS.

> **Field note · I14:** Tying lease renewal to the worker's existing heartbeat —
> instead of adding a dedicated renewal RPC — turned out to dissolve an entire
> earlier problem rather than patch it. Once a live worker's lease renews every
> heartbeat, it can never be wrongly re-picked as "expired," and a genuinely dead
> worker's lease decays exactly when its heartbeat stops. Lease lifetime became a
> synonym for worker liveness, with zero new wire surface.

> **Field note · Phase 4 wrap-up:** Introducing capacity-1 leases (one job per worker
> at a time) quietly made the `BinPacking` strategy behave identically to
> `SimpleFifo` — every candidate worker is either idle or excluded, so there's no
> load differential left to pack against. Rather than paper over it, the team wrote a
> test that pins the degradation as deliberate, and left a per-worker-capacity knob
> as the documented way to bring packing back.

The phase met two of its three defined success criteria outright: two tenants
sharing one worker measurably interleave rather than one starving the other, and a
worker killed mid-job has its work reassigned and completed elsewhere. The third — a
real `bazel build` running against Brokkr as its remote executor — was assessed as
infeasible in the WSL2, Bazel-less development environment and recorded as a tracked
gap rather than quietly skipped, per the project's own rule about honest
exit-criteria reviews.

---

## Phase 5 — Consensus & HA
*Complete. Eleven milestones, PRs #120–#172.*

The plan committed to one architectural decision before a line of code existed:
the Raft state machine would be a **sans-IO core** — pure, single-threaded, and
blind to clocks, threads, and sockets — with a thin shell owning the real clock
and gRPC transport. The reasoning was that a definition of done demanding a
million operations under fault injection with zero divergence is only checkable
if the protocol can be driven by a seeded deterministic loop. That decision held
all the way through, and it is why the certification exists at all.

The phase ran the paper's arc in order: crash-safe hard state on redb (a torn
vote between bumping the term and clearing the vote would let a node vote twice
in one term), leader election, log replication with the **Figure-8 commit rule**,
snapshots and log compaction, joint-consensus membership with learners and a
catch-up gate, and finally a `MetaKv` seam that let the redb store be swapped for
a Raft-replicated one without a single consumer noticing.

**Three test layers, and each caught what the others structurally could not.**
The deterministic simulator turns a failing interleaving into a *seed* — a
permanent fixture. `turmoil` runs the real tonic stack, and caught something no
pure-core simulator can model: a partition that silently drops packets leaves an
HTTP/2 connection "alive" forever, so without keepalive a healed cluster never
re-integrates the peer. And real processes caught what neither could — the
end-to-end test found, on its first two runs, that an HA control plane needs a
worker attached to *every* node (the registry is per-node and deliberately
unreplicated), and that a decision about degrading gracefully on a follower had
been implemented on only half its path, so a follower's cache *lookup* still
killed builds before the action ran.

The numbers: kill the leader and a survivor accepts writes in **291.9 ms**; a
full `brokk run` completes **133.4 ms** after the kill with the cache intact;
and **1,000,000 operations** under partitions, crashes, restarts, membership
churn and continuous compaction finished with **zero divergence**.

The certification's own first attempt is the phase's sharpest lesson. It decayed
from 462 to 188 operations per second and projected past two hours — and the
cause was the *test harness*, which stored the entire applied history inside
every snapshot blob so its oracle could compare histories directly, making
compaction O(history) and the run quadratic. A real state machine snapshots
bounded state. The fix was to the constant, never to the requirement: lowering
the operation count to make a number look better would have made the
certification worthless.

The phase closed by securing the Raft peer plane with mutual TLS, on an
asymmetry worth stating plainly — on the client and worker planes the worst case
is unauthorized access to cache data, but `AppendEntries` on the peer plane
appends to the replicated log, which makes an unauthenticated peer port a write
path into consensus itself.

---

## What repeats across every phase

- **Injected clocks, never `Instant::now()` in logic.** Every time-sensitive method —
  worker eviction, lease expiry, fair-queue tagging — takes an explicit `now`
  parameter, so eviction-boundary tests pin a moment and add durations instead of
  sleeping.
- **Foundation, then wiring, as separate PRs.** Pure data structures (a
  `LeaseTable`, a `FairQueue`, a `Strategy` trait) land fully unit-tested before the
  riskier integration touches live dispatch code.
- **Decorators over rewrites.** New CAS capabilities wrap the existing trait rather
  than modifying it, so nothing upstream has to change to gain a bloom filter or an
  LRU tier.
- **ADRs precede forks, not follow them.** Multi-worker dispatch, leases, tenancy,
  and auth were each written up as an ADR and checked with the owner before
  implementation started, not rationalized after the fact.
- **WSL2 as a partial oracle.** The dev host can't run the full sandbox test suite
  (seccomp argument-filter tests need a real kernel), so verification is per-crate
  locally with Linux CI as the backstop — and a past stale-branch mistake (PR #96)
  is carried forward as a standing reminder to check `origin/main`, not just the
  working tree.
- **The "what surprised me" section is not optional.** Nearly every milestone entry
  ends with one, and they're where the sharpest, most specific engineering lessons
  in the whole project actually live — a discipline that turns each PR into a small,
  searchable case study rather than just a diff.

---

## The project so far

- **5 / 6** phases complete
- **~60** numbered milestones shipped (Phases 2–5)
- **#98–#116** PR range for Phase 4; **#120–#172** for Phase 5
- **14** architecture decision records
- **10** workspace crates, zero dependency cycles
- **1.94** pinned MSRV (bumped twice: 1.78 → 1.85 in Phase 0, 1.85 → 1.94 in
  Phase 6 P0)

**Architecture decision records to date:**

| ADR | Title |
|---|---|
| 0001 | Rust everywhere |
| 0002 | REAPI compatibility |
| 0003 | Embedded KV: redb |
| 0004 | Tracing from day one |
| 0005 | No Docker for sandbox |
| 0006 | Apache-2.0 license |
| 0007 | CAS GC strategy |
| 0008 | Multi-worker scheduling |
| 0009 | Leases & fair scheduling |
| 0010 | Tenants & fair scheduling |
| 0011 | Auth (JWT + mTLS) |
| 0012 | Operator TUI |
| 0013 | Custom Raft consensus |
| 0014 | WASM scheduling policies |

---

## What's next

Five of six phases are complete. Phase 5 met all three of its definition-of-done
lines, and the one rule `CLAUDE.md` singles out — implement Raft from the paper,
never import one — held from the first commit to the millionth operation.

Three things are open, and all three are the owner's call rather than oversights:

- **The Bazel-compatibility test** from Phase 4 (§16 task 9): a real
  `bazel build` against `brokk` as the remote executor. Still untried, because
  the dev environment has no Bazel.
- **The operator TUI** (ADR 0012), pulled forward from Phase 6 as additive and
  read-mostly, accepted on 2026-06-30 and **still unstarted**. Recorded in
  `docs/plan.md` §11.1 so the phase table stays honest about it.
- **Phase 6 itself**, which the plan deliberately leaves as a menu rather than a
  sequence: speculative execution, cross-region replication, GPU scheduling,
  reproducibility verification, a web UI, federation.

Phase 5's own deferred list — pre-vote, leadership transfer, chunked
`InstallSnapshot`, follower reads at the read index, and the rest — is in the
retrospective with the reason each one is safe to defer. The only entry that is a
known inefficiency rather than a missing feature is the simulator's oracle: it
stores full history to compare histories, and a hash chain would make the
certification linear in both time and memory.

---

*Compiled from `docs/plan.md`, `CLAUDE.md`, the ADRs in `docs/architecture/`, the
phase journals in `docs/journal/`, and `CHANGELOG.md`. All dates, PR numbers, and
milestone outcomes are drawn directly from those records.*
