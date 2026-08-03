# Operator TUI — design

**Date:** 2026-08-02
**Implements:** [ADR 0012](../../architecture/0012-operator-tui.md), workstreams WS0 and WS3
**Follows:** [the observability backend](2026-08-02-observability-read-model-design.md), complete and merged (PRs #192–#200)
**Status:** design approved, unstarted

---

## What is already settled, and is not re-litigated here

ADR 0012 decided the TUI's shape: `ratatui` + `crossterm`, a hand-rolled Elm
architecture (`Model` + `Action` + pure `update` + `view`) rather than
`tui-realm`, a `tokio::mpsc` action bus merging input / tick / stream, a
`Component` trait for panels, and a panic hook that restores the terminal. Those
stand.

This spec covers what the ADR could not decide, because the thing the TUI reads
did not exist when it was written.

---

## What changed since the ADR

**The backend is real now.** `ObservabilityService` serves `GetCluster`,
`ListWorkers`, `ListJobs`, `GetJob`, `GetPolicy`, `GetCasStats` and a
`WatchEvents` stream on a dedicated operator listener, aggregated across every
Raft peer. `brokkr_sdk::ObservabilityClient` wraps all of it. The TUI is a
consumer of a finished API rather than a co-design with one.

**One ADR dependency does not do what the ADR thought.** It lists `tui-logger`
for an in-TUI log panel, reasoning it is "near-free given every control path is
already instrumented". That reasoning does not hold: `tui-logger` captures
`tracing` records from **the process it runs in**, and `brokkr-tui` is a
separate binary talking gRPC. The control plane's instrumented paths are in the
control plane's process. A Logs panel fed by `tui-logger` would show the TUI's
own diagnostics.

That is not fatal, because something better exists now: `WatchEvents` *is* the
cluster activity feed — node unreachable, worker added, leadership changed,
policy quarantined. It is what the ADR wanted the Logs panel to be, pushed
rather than polled. So the panel stays and its source changes, and `tui-logger`
is dropped.

---

## Decisions

| # | Decision | Rejected | Why |
|---|---|---|---|
| D1 | **Both `WatchEvents` and a periodic unary sweep feed the model**, each replacing it wholesale. | Stream only; unary polling only | Stream-only is simpler and was the recommendation, but a sweep is a genuine safety net against a delta bug — a class of failure that would otherwise show a confidently wrong console indefinitely. Polling-only reintroduces the per-observer cost the backend's poller design exists to avoid. |
| D2 | **Add a unary `GetSnapshot`** returning the same `SnapshotEvent` payload the stream sends. | Five separate unary calls for the sweep | The `Snapshot` event is atomic — one read of the snapshot. Five RPCs are not: the poller can swap the snapshot between any two, producing a `Replace` with workers from one poll and jobs from the next. `GetSnapshot` makes the sweep genuinely equivalent to a stream snapshot rather than merely similar. |
| D3 | **Panels: Cluster, Workers, Jobs, Events.** Policy folds into Cluster as per-node columns. `tui-logger` dropped. | A separate Policy panel; a `tui-logger` Logs panel; three panels with no Events | See the finding above for Logs. Policy is four numbers and a flag per node, which reads naturally beside node health and does not carry a panel. Dropping Events would lose the "what just happened" view that is most of the point of a *live* console. |
| D4 | **Repeatable `--control`, first that answers.** | A single endpoint | Mirrors `brokk run`, which already takes a repeatable `--control` for exactly this reason. Any node aggregates the whole cluster, so one answering node suffices — but pointing at a single node means seeing nothing when that node is the one that died, which is precisely when you looked. |
| D5 | **Start even when nothing answers**, with a banner and backoff retry. | Exit with a clear error | An operator launching a console during an outage is the expected case, not an error. A console that refuses to open when the cluster is down is useless at the moment it matters most. |
| D6 | **Render proto types directly.** | Re-project into TUI-local view types | The `views` types live in `brokkr-control`, which the TUI must not depend on — that edge would break the DAG. The protos are already the contract, and a third representation of the same data would earn nothing but drift. |
| D7 | **The 3-node integration test lands first**, before any TUI code. | Leaving it at the end, as the backend plan did | It validates code that is *already shipped*. Finding a transport bug before building a UI on top of it is much cheaper than after, and it runs regardless of what happens to the TUI. |

---

## Known limitation, not designed away

**Deltas and sweeps race.** Two `Replace` sources plus one delta source means a
sweep reading at T=10 can be overtaken by a delta emitted at T=5, applied on
top, briefly regressing the model. Within the stream alone this cannot happen —
a single ordered source.

It is bounded: the next delta or the next sweep corrects it, so the window is at
most one sweep interval. `as_of` is rendered in the header so staleness is
visible rather than implied. A clean fix would need timestamps on deltas, which
they do not carry, and inventing them to close a self-correcting window is not
worth the wire change. **Stated here rather than hidden**, so the next person
does not rediscover it as a bug.

---

## Architecture

A new binary crate `brokkr-tui`, depending on `brokkr-sdk` and `brokkr-proto`
only. Never on `brokkr-control`: that edge would break the DAG, and the console
should build without the server.

| File | Responsibility |
|---|---|
| `main.rs` | flags, terminal lifecycle, panic hook |
| `app.rs` | `Model`, `Action`, `update()` — pure, no I/O |
| `conn.rs` | connection actor: owns the SDK client, emits `Action`s |
| `panels/mod.rs` | the `Component` trait, shared table styling |
| `panels/cluster.rs` | nodes, Raft state, policy columns |
| `panels/workers.rs` | worker table |
| `panels/jobs.rs` | recent jobs |
| `panels/events.rs` | bounded activity feed |

### The model has exactly two kinds of input

This is what makes D1 safe:

```rust
enum Action {
    /// Replace the world. From the stream's opening/resync Snapshot, OR from
    /// the periodic GetSnapshot sweep. Both are authoritative and, thanks to
    /// D2, both are atomic.
    Replace(Box<bv1::SnapshotEvent>),
    /// Apply one delta. Only ever from the stream.
    Apply(bv1::ClusterEvent),
    /// Connection state changed — drives the banner.
    Link(LinkState),
    /// Terminal input.
    Key(crossterm::event::KeyEvent),
    /// Render tick.
    Tick,
}

struct Model {
    snapshot: bv1::SnapshotEvent,
    events: VecDeque<StampedEntry>, // bounded ring of 1000; cluster + local
    link: LinkState,
    focus: Panel,
    scroll: [usize; 4],
}
```

There is **one merge rule per kind of input, not one per source**. A sweep and a
stream `Snapshot` are indistinguishable to `update()`, which is exactly why they
cannot disagree about *how* to merge. `update()` is pure and reads no clock — a
`Tick` carries its own timestamp for anything that needs one.

### Data flow

```text
  crossterm::EventStream ─┐
  render ticker ──────────┼─▶ mpsc<Action> ─▶ update(&mut Model, Action) ─▶ view()
  connection actor ───────┘
        │
        ├─ watch_events()  ─▶ Snapshot ▶ Replace,  delta ▶ Apply
        └─ GetSnapshot @ 10s ─────────▶ Replace
```

### The connection actor

Owns everything fallible, so `update()` does not have to:

```text
for endpoint in endpoints.cycle():
    connect              → Link(Connecting)
    watch_events()       → first item is Snapshot → Replace
    loop: delta          → Apply
    on error             → Link(Disconnected{reason}) → backoff → next endpoint
```

Endpoint rotation reuses the shape of `brokkr-worker`'s `rotation_plan`: the
whole first cycle at zero delay, then exponential backoff per completed cycle,
capped. Reusing the shape rather than the code — it lives in `brokkr-worker`,
which the TUI does not depend on — and it is pure, so it is tested the same way.

**Reconnect needs no special handling.** A dropped stream rotates to the next
endpoint and reconnects, which yields a fresh `Snapshot`, which replaces the
model. It is the same path as first connect.

The sweep is a second task on a 10s timer calling `GetSnapshot`. Ten seconds
because the backend already polls peers at 2s: the sweep is a **safety net
against a delta bug**, not a freshness mechanism, and making it faster would add
load without adding information.

---

## Panels

All four read the same `Model.snapshot`.

- **Cluster** — one row per node: id, role, term, commit / applied, reachable,
  plus policy columns (loaded, quarantined, decided, declined). Header carries
  degraded state, `as_of`, and "2 of 3 nodes reporting".
- **Workers** — id, owning node, labels, in-flight, last seen, stale.
- **Jobs** — newest first: id, tenant, state, worker, exit code, age.
- **Events** — bounded ring of the last 1000, newest first, each stamped with
  arrival time. The events themselves carry no timestamp; only the snapshot
  does, so arrival time is the honest thing to show.

  It carries **two kinds of entry**, because dropping `tui-logger` otherwise
  leaves the TUI's own diagnostics with nowhere to go:

  ```rust
  enum Entry {
      /// A delta from `WatchEvents`.
      Cluster(bv1::ClusterEvent),
      /// Something the console itself noticed — connected to node-2, sweep
      /// failed, reconnecting.
      Local(String),
  }
  ```

  Both belong in one feed and are visually distinguished rather than separated.
  "Sweep failed" immediately above "node-2 unreachable" is the sequence an
  operator needs to read as one story; two panels would make them hunt for the
  correlation.

`Tab` / `Shift-Tab` cycle panels, arrows scroll within, `q` quits, `?` shows
help. Deliberately plain tables: layout is the part of this least likely to
match the owner's taste sight-unseen, and guessing at something fancier would
cost more to undo than to add later.

---

## Error handling

Every case degrades rather than exits.

| Case | Behaviour |
|---|---|
| No endpoint answers at startup | UI opens, banner reads disconnected, retry continues (D5) |
| Stream drops | Banner, rotate endpoint, reconnect — same path as first connect |
| Sweep fails | A `Local` entry in the Events panel, model untouched; the stream is still authoritative |
| Panic anywhere | Hook restores the terminal *before* unwinding, so a crash never leaves a wedged shell |
| Terminal too small | Render a "needs N×M" message rather than panicking on a zero-width rect |

---

## Testing

| Layer | Approach |
|---|---|
| `update()` | pure — every `Action` against a fixture `Model`, no terminal |
| replace-vs-apply | a `Replace` after divergent deltas yields the same model as a fresh one |
| endpoint rotation | pure, like `rotation_plan` |
| panels | `TestBackend` buffer snapshots, **empty and disconnected states only** |
| panic restore | see below |
| 3-node aggregation | real sockets, `#[ignore]`d (T1) |

**`TestBackend` snapshots are deliberately narrow.** Only the empty and
disconnected states, which are stable and where a regression actually hurts — a
panic on a zero-width rect, a missing banner. Snapshotting every panel with data
would break on every layout tweak and train people to regenerate them without
reading, which is worse than not having them.

**Panic restore is tested at the seam.** The restore routine is a plain function
unit tested directly, and a second test asserts the hook is installed. Whether
the hook *fires* correctly is verified by hand against a real terminal once and
recorded as a manual check. Panic hooks are global process state and asserting
one fires means panicking inside the harness; claiming an automated test here
would be claiming something not cleanly writable.

---

## Sequencing

| # | Increment | Depends on | DoD |
|---|-----------|------------|-----|
| T1 | 3-node aggregation integration test | — | 1, 2 |
| T2 | `GetSnapshot` RPC + SDK method | — | — |
| T3 | `brokkr-tui` scaffold: lifecycle, panic hook, TEA skeleton, action bus, `Component` | — | 6 |
| T4 | Connection actor: endpoint rotation, stream, sweep | T2, T3 | 3, 4, 7 |
| T5 | Cluster + Workers panels | T4 | 5 |
| T6 | Jobs + Events panels | T4 | 5 |
| T7 | Docs; manual terminal verification recorded | T5, T6 | 6 |

T1 and T2 are independent of everything else and can land immediately. T3 has no
backend dependency at all, so it can proceed even if T1 turns up a transport bug.

---

## Definition of done

1. A 3-node cluster's workers, jobs, CAS and policy all appear from any node,
   each labelled with its owning node. *(T1 proves the backend; T5/T6 the display)*
2. Killing one node leaves the console working, showing the survivor count and
   marking the dead node unreachable. *(T1, T5)*
3. A leadership change appears in the Events panel without waiting for the
   sweep. *(T4, T6)*
4. Killing the *connected* node fails over to the next `--control` endpoint and
   the console keeps working. *(T4)*
5. Every panel renders with a populated cluster and with an empty one, and
   neither panics. *(T5, T6)*
6. A panic restores the terminal — restore routine unit tested, hook
   installation asserted, real-terminal behaviour verified by hand and recorded.
   *(T3, T7)*
7. Starting with no reachable endpoint opens the UI with a banner rather than
   exiting. *(T4)*

---

## Out of scope

Named, so they read as decisions rather than omissions.

- **All mutating actions.** Read-only, as ADR 0012 specifies.
- **The V1 panels from the ADR:** job detail with live log tail, digest
  inspector tree (`tui-tree-widget`), replication ring.
- **Streaming control-plane logs into the TUI.** The `tui-logger` finding above
  explains why the obvious approach does not work; doing it properly needs a
  log-streaming RPC, which is its own decision.
- **Colour themes, mouse support, configurable layouts.** Plain tables first.
- **The web gateway.** Still wraps the same `views` functions; still not built.

---

## References

- [ADR 0012](../../architecture/0012-operator-tui.md) — the accepted decision
- [The observability backend design](2026-08-02-observability-read-model-design.md) — what this consumes
- `docs/superpowers/plans/2026-08-02-observability-backend.md` — the backend plan (complete)
- `crates/brokkr-sdk/src/observability.rs` — the client this builds on
