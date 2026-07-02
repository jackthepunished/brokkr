# Raft — Implementation Notes

Working notes taken while reading **"In Search of an Understandable Consensus
Algorithm (Extended Version)"** — Diego Ongaro & John Ousterhout, 2014
(<https://raft.github.io/raft.pdf>), plus the relevant chapters of Ongaro's PhD
thesis, *Consensus: Bridging Theory and Practice* (2014).

These notes are the **implementation reference** for the from-scratch
`brokkr-raft` crate (Phase 5, `docs/plan.md` §17). Section-number citations like
"§5.2" refer to the paper. Where a rule maps to a concrete Brokkr milestone
(I2–I9, see the Phase 5 loop plan) it is called out inline.

CLAUDE.md rule 10: **no existing Raft crate.** Everything below is implemented by
hand against these notes. If any implementation choice is not pinned by this
document or ADR 0013, stop and ask the owner rather than guessing.

---

## 1. Why Raft (motivation)

- Replicated state machine (RSM): identical deterministic state machines on N
  servers each apply the **same commands in the same order** and therefore stay
  in lockstep. The problem reduces to agreeing on the *log* of commands.
- Paxos is provably correct but notoriously hard to understand and to turn into
  a real system. Raft's whole thesis is **understandability**: same safety and
  performance as (multi-)Paxos, decomposed into pieces a human can hold in their
  head — **leader election**, **log replication**, **safety**, plus membership
  changes and snapshots.
- Raft's simplifying moves:
  1. **Strong leader.** Log entries only flow leader → followers. No entry ever
     flows the other way. This removes a huge class of Paxos edge cases.
  2. **Leader election** using randomized timeouts — the only randomized part.
  3. **Membership changes** via *joint consensus* (overlapping majorities) so
     the cluster keeps serving during reconfiguration.

---

## 2. Server states & terms

### 2.1 The three states (§5.1)

Every server is in exactly one of:

| State         | Role |
|---------------|------|
| **Follower**  | Passive. Issues no RPCs; only *responds* to RequestVote / AppendEntries / InstallSnapshot. Redirects clients to the leader. Every server starts here. |
| **Candidate** | Transient. Trying to become leader for a new term (§5.2). |
| **Leader**    | Handles all client requests, replicates the log, sends heartbeats. Exactly one per term (at most). |

Normal operation is **one leader, everyone else follower.** State transitions
(paper Figure 4):

```
              times out,
              starts election                 receives votes
   ┌────────┐ ─────────────────► ┌───────────┐ from majority ┌────────┐
   │Follower│                     │ Candidate │ ─────────────►│ Leader │
   └────────┘ ◄───────────────── └───────────┘               └────────┘
        ▲   discovers leader or        │  times out,                │
        │   higher term                │  new election              │
        │                              ▼                            │
        └──────────────────────────────────────────────────────────┘
              discovers server with higher term  (→ step down to Follower)
```

### 2.2 Terms as a logical clock (§5.1)

- Time is divided into **terms** of arbitrary length, numbered with consecutive
  integers. Each term begins with an **election**. A term has **at most one
  leader** (Election Safety, below) — some terms have *no* leader (split vote →
  the term ends with no leader and a new term/election starts).
- Terms are Raft's **logical clock**: they let servers detect stale leaders and
  stale information.
- **Term exchange on every RPC.** Each request and response carries the sender's
  `term`. The two universal rules (apply *before* any RPC-specific logic):
  - **Receiver sees a larger term** than its `currentTerm` → set
    `currentTerm ← T`, **convert to follower**, clear `votedFor`.
  - **Receiver sees a smaller term** than its `currentTerm` → **reject** the RPC
    (respond with its own larger `currentTerm` so the stale sender steps down).
- Corollary: a leader or candidate that has been partitioned away discovers it
  is stale the moment it exchanges a single RPC with an up-to-date server, and
  immediately steps down. No leader ever *knowingly* acts while superseded.

> Brokkr: `Term(u64)` and `NodeId` are newtypes (CLAUDE.md invariant). The
> "higher term → step down, clear votedFor" check is centralized so every RPC
> handler runs it first — a common source of subtle election bugs when scattered.

---

## 3. Persistent vs. volatile state (paper Figure 2)

**Persistent state on all servers** — MUST be durable on stable storage
**before responding** to any RPC that changed it (this is the *persist-before-respond*
rule; Brokkr milestone **I2**, backed by redb):

| Field         | Meaning |
|---------------|---------|
| `currentTerm` | Latest term the server has seen (init 0, increases monotonically). |
| `votedFor`    | `candidateId` that received this server's vote in `currentTerm`, or null. |
| `log[]`       | Log entries. Each holds `{ term, command }`. **First index is 1.** |

**Volatile state on all servers:**

| Field         | Meaning |
|---------------|---------|
| `commitIndex` | Highest log index known committed (init 0). |
| `lastApplied` | Highest log index applied to the state machine (init 0). |

**Volatile state on leaders** — reinitialized after every election:

| Field           | Meaning |
|-----------------|---------|
| `nextIndex[]`   | For each peer, index of the next entry to send it (init = leader `lastLogIndex + 1`). Optimistic guess. |
| `matchIndex[]`  | For each peer, highest index known replicated on it (init 0). Truth, monotonic. |

**Persistence discipline (§5.3, and thesis §3.8 on correctness):**
- Any change to `currentTerm`, `votedFor`, or `log[]` must hit durable storage
  before the server sends a reply that depends on it. Otherwise a crash-restart
  could make the server "forget" a vote it already promised or entries it
  already acknowledged, breaking Election Safety / Leader Completeness.
- `commitIndex` and `lastApplied` are **not** persisted — they are safely
  recomputed after restart (commitIndex re-derives from the leader; lastApplied
  is rebuilt by replaying, or starts from the last snapshot's index).

> Brokkr I2: redb write txn commits (fsync) the three persistent fields before
> the handler returns. Crash tests kill the process mid-write and assert the
> node recovers a consistent `(currentTerm, votedFor, log)` — never a torn
> vote. See §9 on snapshots for where `lastApplied` restarts from.

---

## 4. Leader election (§5.2) — Brokkr I3

- **Heartbeats.** The leader sends periodic empty `AppendEntries` (no entries)
  to all followers to assert authority and suppress their election timers.
- **Election timeout.** A follower that receives no valid `AppendEntries` from
  the current leader (and grants no vote) within its **randomized election
  timeout** converts to candidate.
- **Starting an election** (candidate rules):
  1. Increment `currentTerm`.
  2. Vote for **self**.
  3. Reset the election timer.
  4. Send `RequestVote` to all other servers (in parallel).
- **Outcomes of an election:**
  - **Wins:** receives votes from a **majority** of the *whole cluster* (not
    just responders) → becomes leader → immediately sends heartbeats.
  - **Another server establishes leadership:** receives an `AppendEntries` whose
    `term ≥ currentTerm` → recognizes the leader, converts to follower. (If the
    term is smaller, reject and stay candidate.)
  - **Timeout / split vote:** no winner before the timer fires → increment term,
    start a *new* election.

### 4.1 Randomized timeouts — the split-vote fix (§5.2, §9.3)

- Split votes happen when several followers time out simultaneously and each
  becomes a candidate for the same term, splitting the vote so nobody gets a
  majority.
- Fix: choose each election timeout **randomly** from a fixed interval
  (paper uses **150–300 ms**). This de-synchronizes candidates, so usually one
  starts first, wins, and sends heartbeats before the others time out.
- The timeout is re-randomized **each** time the timer is reset.
- Timing requirement for a stable leader:
  **`broadcastTime ≪ electionTimeout ≪ MTBF`.**
  broadcastTime = one round of RPCs (~0.5–20 ms on real networks);
  electionTimeout = the 150–300 ms window; MTBF = mean time between failures of
  a single server (months). The DoD "<2 s to re-elect" (§17) is comfortably met
  by this interval plus a couple of retries.

> Brokkr I3: election timeout is drawn from an **injected RNG** (seeded) and the
> clock is **injected** — never `SystemTime::now()` in the state machine — so
> `turmoil` runs are deterministic (`docs/plan.md` §21: "no `SystemTime::now()`
> in tests; use injected clocks"). Tests: single-node self-elects; 3-node happy
> election; forced split vote resolves; higher-term RequestVote makes a leader
> step down.

### 4.2 RequestVote RPC (Figure 2)

**Arguments:** `term`, `candidateId`, `lastLogIndex`, `lastLogTerm`.
**Results:** `term` (voter's currentTerm, for the candidate to update itself),
`voteGranted` (bool).

**Receiver implementation:**
1. Reply `false` if `term < currentTerm`.
2. If (`votedFor` is null **or** `votedFor == candidateId`) **and** the
   candidate's log is **at least as up-to-date** as the receiver's log
   (§5.4.1 election restriction, §6 below) → grant vote, record `votedFor`,
   **reset the election timer** (granting a vote counts as hearing from a
   legitimate candidate).

Notes / gotchas:
- The `votedFor == candidateId` clause makes RequestVote **idempotent** under
  retransmission: a dropped-then-resent vote request from the *same* candidate
  in the *same* term is granted again, not denied.
- Persist `votedFor` (and any `currentTerm` bump) **before** replying
  `voteGranted = true`. A crash that loses the vote could let the term elect two
  leaders → violates Election Safety.

---

## 5. Log replication (§5.3) — Brokkr I4

- Each log entry stores `{ index, term, command }`. `index` is its position
  (1-based); `term` is the leader's term when the entry was *created*. `command`
  is opaque to Raft — it is applied to the replicated state machine.
- **Client request flow:** leader appends the command to its log, then issues
  `AppendEntries` in parallel to followers. Once the entry is **committed** (see
  §5.1 commit rule below), the leader applies it to its state machine and
  returns the result to the client. If followers are slow/down/lossy the leader
  **retries indefinitely** (even after replying to the client, to make every log
  eventually converge).
- **Commit definition:** an entry is committed once the leader that created it
  has replicated it on a **majority** of servers. Committing an entry commits
  **all preceding entries** in the leader's log (including entries from previous
  leaders) — subject to the current-term restriction in §7.

### 5.1 AppendEntries RPC (Figure 2)

**Arguments:** `term`, `leaderId`, `prevLogIndex`, `prevLogTerm`, `entries[]`,
`leaderCommit`.
**Results:** `term`, `success` (true iff the follower had a matching entry at
`prevLogIndex`/`prevLogTerm`).

**Receiver implementation (order matters):**
1. Reply `false` if `term < currentTerm`.
2. Reply `false` if the log has **no** entry at `prevLogIndex` whose term equals
   `prevLogTerm` (the *consistency check*).
3. If an existing entry **conflicts** with a new one (same index, different
   term), **delete the existing entry and everything after it**.
4. **Append** any new entries not already present.
5. If `leaderCommit > commitIndex`, set
   `commitIndex ← min(leaderCommit, index of last new entry)`.

Critical subtleties:
- Steps 3–4 are **not** "truncate to prevLogIndex then append." You only delete
  on an actual *conflict*. A delayed/duplicated AppendEntries must not truncate
  entries the follower already correctly holds beyond the ones in this request —
  doing so can erase committed entries and break Log Matching. Only remove
  entries that genuinely disagree in term.
- Heartbeats (`entries` empty) still run steps 1, 2 and 5 — they advance the
  follower's `commitIndex` and act as the consistency probe.

### 5.2 The Log Matching Property (§5.3)

Two guarantees, together they give the property:
- If two entries in different logs have the **same index and term**, they store
  the **same command**. (A leader creates at most one entry per index per term,
  and never moves an entry.)
- If two entries in different logs have the same index and term, then the logs
  are **identical in all preceding entries**. (Induction, enforced by the
  AppendEntries consistency check in step 2: an AppendEntries only succeeds if
  the follower's entry at `prevLogIndex` matches, which inductively guarantees
  everything before it matches too.)

### 5.3 Repairing follower logs (§5.3)

After a sequence of leader crashes, follower logs can diverge (missing entries,
extra uncommitted entries, or both). The leader forces convergence:

- Leader keeps `nextIndex[peer]`. It sends AppendEntries starting there.
- On **success**: `matchIndex[peer] ← prevLogIndex + len(entries)`,
  `nextIndex[peer] ← matchIndex + 1`.
- On **failure** (consistency check failed): **decrement** `nextIndex[peer]` and
  retry, walking backwards until a matching entry is found; from there the
  follower's conflicting tail is overwritten (steps 3–4) to match the leader.
- **Optimization** (paper §5.3 footnote / thesis): the follower can return the
  `term` of the conflicting entry and the first index it stores for that term,
  letting the leader skip a whole term's worth of indices per round trip instead
  of decrementing by one. Nice-to-have; correctness does not depend on it.
- **Leader Append-Only:** a leader **never** overwrites or deletes entries in
  *its own* log; it only appends. All truncation happens on followers.

> Brokkr I4 tests: follower with a missing suffix gets backfilled; follower with
> a conflicting uncommitted tail gets it overwritten; commitIndex advances only
> on majority; heartbeat propagates leaderCommit. Plus the Figure-8 regression
> test (§7).

---

## 6. Election restriction — "up-to-date" (§5.4.1) — Brokkr I3

To guarantee a new leader already holds **all committed entries** (so it never
has to *receive* a committed entry, only send them — consistent with the
strong-leader model), Raft restricts *who can win* an election:

- A voter **denies** its vote if **its own log is more up-to-date** than the
  candidate's log. RequestVote carries the candidate's `lastLogIndex` /
  `lastLogTerm` precisely for this check.
- **"More up-to-date" comparison** between two logs, using the *last* entries:
  1. The log with the **higher last-entry term** is more up-to-date.
  2. If last-entry terms are **equal**, the **longer** log (higher last index)
     is more up-to-date.
- Because committing requires a majority and winning requires a majority, the
  two majorities intersect in at least one server; that server's vote enforces
  that the winner's log is at least as up-to-date as any committed entry.
  Therefore **the leader for a term contains every entry committed in prior
  terms** — this is used to prove Leader Completeness.

> Brokkr I3: implement the comparator as a total order on
> `(lastLogTerm, lastLogIndex)` and unit-test the four cases (higher term wins;
> equal term longer wins; equal → grant; stale term → deny). This comparator is
> also reused by the candidate to decide it is eligible.

---

## 7. Committing entries from previous terms — **the Figure-8 rule** (§5.4.2)

**This is the single most important safety subtlety in Raft.** Getting it wrong
produces a system that passes casual testing and silently loses committed data.

The naive commit rule — "an entry is committed once stored on a majority" —
is **unsafe for entries from *earlier* terms.** Paper Figure 8 walks a concrete
5-node scenario:

1. `S1` (leader, term 2) partially replicates an entry at index 2 to `S1,S2`.
2. `S1` crashes. `S5` wins term 3 (votes from `S3,S4,S5` — allowed, its log is
   as up-to-date as theirs) and writes a *different* entry at index 2.
3. `S5` crashes. `S1` restarts, wins term 4, and continues replicating its old
   term-2 index-2 entry to a **majority** (`S1,S2,S3`). By the naive rule this
   term-2 entry now looks "committed."
4. But if `S1` crashes again, `S5` can *still* win term 5 (votes from
   `S2,S3,S4,S5` are possible depending on logs) and **overwrite** index 2 with
   its term-3 entry — destroying an entry we called committed. **Safety
   violation.**

**The fix — Raft never commits entries from previous terms by counting
replicas:**

> A leader only considers an entry committed once an entry **from its own
> current term** has been stored on a majority. When such a current-term entry
> commits, **all preceding entries commit indirectly** via the Log Matching
> Property.

Concretely, the leader commit-advance rule (Figure 2, "Rules for Servers →
Leaders"):

> Set `commitIndex ← N` for the largest `N` such that
> **`N > commitIndex`**, **a majority of `matchIndex[i] ≥ N`**, **and
> `log[N].term == currentTerm`.**

The `log[N].term == currentTerm` clause is the whole game. A leader that inherits
uncommitted entries from prior terms must **not** mark them committed on
replica-count alone; it waits until one of *its own* entries reaches a majority,
which then sweeps the older entries in with it. In practice the leader appends a
**no-op entry** at the start of its term so it can commit quickly and learn its
true commit index (also needed for safe read-only queries, §11).

> Brokkr I4 **must** include a deterministic Figure-8 regression test: script the
> exact term-2/term-3/term-4/term-5 leadership hand-offs above with a fixed seed
> and assert the term-2 entry is **never** reported committed until a term-4
> (current-term) entry commits over it — and that no committed index is ever
> overwritten. This test is the proof that the current-term clause is honored.

---

## 8. The five safety properties (paper Figure 3)

Every one of these is an invariant `brokkr-raft` must never violate; each maps to
tests / simulation checks.

| # | Property | Statement | Enforced by |
|---|----------|-----------|-------------|
| 1 | **Election Safety** | At most one leader per term. | One vote per server per term + majority to win (§5.2); `votedFor` persisted (§3). |
| 2 | **Leader Append-Only** | A leader never overwrites/deletes its own log entries — only appends. | §5.3 leader rules; truncation only on followers. |
| 3 | **Log Matching** | If two logs share an entry at some index+term, they are identical up to and including that index. | AppendEntries consistency check §5.1 step 2; one entry per index per term. |
| 4 | **Leader Completeness** | If an entry is committed in a term, it is present in the logs of all leaders of higher terms. | Election restriction §6 + current-term commit rule §7. |
| 5 | **State Machine Safety** | If a server applied an entry at index `i`, no other server ever applies a *different* entry at index `i`. | Follows from 1–4; the top-level correctness guarantee. |

**State Machine Safety** is what the linearizability check in the simulation
suite ultimately verifies (I5, I9): every server applies the same command
sequence, so the replicated KV never diverges — matching the DoD "1M operations
under fault injection with zero divergence" (§17).

---

## 9. Log compaction — snapshots (§7) — Brokkr I6

The log cannot grow forever. Compaction discards a prefix of applied entries and
replaces it with a **snapshot** of the state machine.

- **Independent snapshots.** Each server snapshots on its own (no leader
  coordination needed for the common case) covering entries **up to and
  including `lastApplied`** — only committed, applied state is ever snapshotted.
- **Snapshot contents:**
  - `lastIncludedIndex` — index of the last entry the snapshot replaces.
  - `lastIncludedTerm` — that entry's term (kept for the AppendEntries
    consistency check of the entry that now *follows* the snapshot).
  - the serialized **state-machine state** at that index.
  - the **latest cluster configuration** as of that index (needed so membership
    survives compaction — ties into §10).
- After writing the snapshot durably, the server **discards** log entries through
  `lastIncludedIndex` and any older snapshot. `lastApplied` / `commitIndex`
  restart from `lastIncludedIndex` after a restart.
- **InstallSnapshot RPC** — used only when a follower is so far behind that the
  leader has **already discarded** the next entry that follower needs
  (i.e. `nextIndex[peer] ≤ leader.lastIncludedIndex`):

  **Arguments:** `term`, `leaderId`, `lastIncludedIndex`, `lastIncludedTerm`,
  `offset`, `data[]` (chunk), `done`.
  **Results:** `term`.
  **Receiver:** reject stale term; write chunks by `offset`; on `done`, install
  the snapshot as the new state-machine base, discard any log prefix it covers,
  and — if an existing entry matches `lastIncludedIndex`/`lastIncludedTerm` —
  keep the log suffix after it; otherwise discard the whole log.

- Snapshotting is comparatively rare, but implementations must not block normal
  operation while snapshotting (copy-on-write / background write). Brokkr's redb
  state machine gives this naturally: a snapshot *is* the redb file at index N
  (see ADR 0003's "snapshot format becomes the redb file at log index N").

> Brokkr I6 tests: a lagging follower that the leader can no longer feed from the
> log is caught up via InstallSnapshot; snapshot + truncate preserves the
> consistency check for the first post-snapshot AppendEntries; restart-from-
> snapshot yields identical applied state.

---

## 10. Cluster membership changes — joint consensus (§6) — Brokkr I7

Changing the set of servers cannot be done by switching every node from
`C_old` to `C_new` at once: nodes switch at different times, so for a window
**two disjoint majorities** could exist (one under `C_old`, one under `C_new`),
electing **two leaders in the same term** → Election Safety violation.

Raft's answer is a **two-phase, log-based** transition using a *joint*
configuration `C_old,new`:

1. Leader receives a reconfiguration request; it appends a **`C_old,new`** config
   entry to its log and replicates it. A server **adopts a configuration entry as
   soon as it appears in the log** (not when committed) — this is the one place
   Raft acts on an *uncommitted* entry, and it is safe because of the overlap
   below.
2. While in `C_old,new`, **agreement (elections *and* commitment) requires a
   majority of `C_old` AND, separately, a majority of `C_new`.** Overlapping
   majorities make two competing leaders impossible during the switch.
3. Once `C_old,new` is **committed**, the leader appends a **`C_new`** config
   entry. From when a server adopts `C_new`, agreement needs only a `C_new`
   majority. Once `C_new` commits, the reconfiguration is done; servers not in
   `C_new` can shut down.

Three practical issues the thesis calls out (all in scope for I7 to at least
handle correctly):
- **New servers start empty** → they can stall commitment while catching up.
  Fix: add them as **non-voting** members first; they receive the log but do not
  count toward majorities until caught up, *then* run the joint-consensus change.
- **A removed leader** (leader not in `C_new`): it steps down once `C_new` is
  committed. It must keep replicating `C_new` even though it is not part of it,
  and must not count its own vote for `C_new` commitment.
- **Removed servers can disrupt** the cluster by timing out and sending
  RequestVotes for higher terms. Fix: servers **ignore RequestVotes received
  within the minimum election timeout** of hearing from a current leader
  (a "pre-vote"-adjacent heuristic in the thesis).

> Brokkr I7: implement joint consensus exactly as above — **do not** take the
> single-server-at-a-time shortcut (thesis §4.3) without asking the owner first;
> the loop plan pins "membership via joint consensus." Tests: add a node
> (catch-up → C_old,new → C_new); remove a node; kill the old-majority mid-switch
> and assert no split brain.

---

## 11. Client interaction & linearizability (§8) — Brokkr I8

- **Find the leader.** A client sends commands to the leader. If it hits a
  follower (or an ex-leader), the server **redirects** it to the current leader
  (or the client retries a random node until it finds one). This is the
  **leader-redirect** in Brokkr I8.
- **Linearizable semantics:** each command appears to execute **exactly once**,
  atomically, at some point between its call and its response.
- **Exactly-once under retries.** Leader failover means a client may resend a
  command whose commit it never heard about, risking double-apply. Fix: the
  client tags each command with a **unique serial number**; the state machine
  records, per client, the **latest serial applied and its result**, and on a
  duplicate returns the cached result **without re-applying**. (Session state; in
  Brokkr this rides in the replicated KV state machine.)
- **Read-only queries without a log write** (optimization, must be done
  carefully):
  1. At the **start of its term** the leader commits a **no-op** entry so it
     learns which entries are actually committed (ties to §7 — a fresh leader
     doesn't yet know its commit index for prior-term entries).
  2. The leader records `readIndex = commitIndex`, then **confirms it is still
     leader** by exchanging a round of heartbeats with a majority *before*
     answering (guards against a superseded leader serving a stale read).
  3. It waits until `lastApplied ≥ readIndex`, then serves the read from the
     state machine.

> Brokkr I8 (STOP AND ASK first — the loop pins "which state replicates"): put
> the replicated KV behind a trait in `brokkr-control`, with leader redirect and
> the dedup session cache. Before implementing, confirm with the owner exactly
> which control-plane state (worker registry? action-cache metadata? leases?)
> moves under Raft vs. stays local.

---

## 12. How the pieces meet the Phase 5 DoD (§17)

| DoD clause | Mechanism in these notes | Brokkr milestone / test |
|------------|--------------------------|-------------------------|
| Kill leader → new leader in **< 2 s** | Randomized 150–300 ms election timeout + heartbeats (§4.1). | I3 election tests; I9 timed kill. |
| Partition → **minority stops accepting writes** | Commit needs a majority; a minority leader can never reach majority `matchIndex`, so it never commits and (on heartbeat rejection / higher term) steps down (§2.2, §5). | I5 partition sim; I9 minority-write-rejection. |
| Rejoin → **consistent** | Log repair via `nextIndex` back-off + conflict truncation (§5.3); InstallSnapshot if truncated (§9). | I4/I6; I9 rejoin check. |
| **1M ops under faults, zero divergence** | State Machine Safety (§8) from the five properties + the Figure-8 current-term commit rule (§7). | I5 linearizability check; I9 Jepsen-style harness. |

---

## 13. Implementation checklist (derived from the notes)

Ordered roughly by Brokkr milestone. Each item gets a test in the *same* commit.

- [ ] `Term(u64)`, `LogIndex(u64)`, `NodeId(...)` newtypes; `LogEntry { term, command }`. **(I1)**
- [ ] Persistent `(currentTerm, votedFor, log)` on redb; persist-before-respond; crash tests. **(I2)**
- [ ] Centralized "higher term → step down + clear votedFor" pre-check on every RPC. **(I2/I3)**
- [ ] Election timer with **injected seeded RNG + injected clock** (150–300 ms). **(I3)**
- [ ] RequestVote handler + `(lastLogTerm, lastLogIndex)` up-to-date comparator. **(I3)**
- [ ] AppendEntries handler: 5 steps, conflict-only truncation, commitIndex advance. **(I4)**
- [ ] Leader replication loop: `nextIndex`/`matchIndex`, back-off on failure. **(I4)**
- [ ] Leader commit rule with the **`log[N].term == currentTerm`** clause + no-op on election. **(I4)**
- [ ] **Figure-8 regression test** (fixed seed). **(I4)**
- [ ] `turmoil` simulation: partitions, reorder, crash-mid-write, linearizability oracle. **(I5)**
- [ ] Snapshot (lastIncludedIndex/Term + state + config) + InstallSnapshot RPC. **(I6)**
- [ ] Joint-consensus membership (`C_old,new` → `C_new`), non-voting catch-up, disruption guard. **(I7)**
- [ ] Raft-backed KV behind a trait in `brokkr-control` + leader redirect + dedup session cache. **(I8)**
- [ ] 3-node HA + Jepsen-style fault harness proving the DoD. **(I9)**

---

## 14. Common pitfalls to avoid (thesis §3.6, and hard-won folklore)

1. **Committing prior-term entries by replica count.** The #1 correctness bug —
   see §7. Always gate on `log[N].term == currentTerm`.
2. **Truncating the log on every AppendEntries.** Truncate on *conflict only*
   (§5.1) or you erase committed entries under reordering/duplication.
3. **Forgetting to persist before replying.** A lost `votedFor` re-elects a
   second leader for a term; a lost log tail un-acks a "committed" entry.
4. **Counting votes/replicas against responders instead of the full cluster.**
   Majority is of *all* configured servers, not of those currently reachable.
5. **Not resetting the election timer** on a granted vote or a valid
   AppendEntries — causes needless elections and leadership churn.
6. **Using wall-clock time inside the state machine.** Breaks determinism and
   `turmoil` reproducibility; inject the clock.
7. **Acting on a config change only when committed.** Config entries take effect
   **when appended** (§10) — the one deliberate exception.
8. **A stale leader serving reads.** Read-only queries need the heartbeat-
   confirm step (§11), else a partitioned ex-leader returns stale data.

## 15. References

- **Raft (extended), Ongaro & Ousterhout, 2014** — <https://raft.github.io/raft.pdf>
  (Figures 2, 3, 4, and 8 are the load-bearing ones; keep them open while coding.)
- **Ongaro PhD thesis**, *Consensus: Bridging Theory and Practice*, 2014 —
  membership changes (Ch. 4), client interaction & read-only queries (Ch. 6),
  correctness proofs (Ch. 3/8).
- **Raft visualizations / lecture** — <https://raft.github.io/>,
  <https://www.youtube.com/watch?v=YbZ3zDzDnrw>.
- **Prior art to steal from (read, don't copy — rule 10):** etcd `raft/`, TiKV
  `raftstore` (`docs/plan.md` §28 Tier 4). Jepsen analyses for the fault model
  the I9 harness must survive — <https://jepsen.io/analyses>.
- Brokkr: `docs/plan.md` §17 (Phase 5 tasks + DoD), §21 (testing / turmoil),
  ADR 0003 (redb → Raft snapshot path), forthcoming ADR 0013 (Raft design).
