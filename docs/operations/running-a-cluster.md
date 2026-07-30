# Running a local Brokkr cluster

A minimal Brokkr cluster is two processes — one **control plane**
(`brokkr-control`) and one or more **workers** (`brokkr-worker`) — that clients
reach through the `brokk` CLI (or the REAPI gRPC API directly). This is the
same topology the `two_process_cluster` integration test exercises.

> Workers run actions on **Linux** (the sandbox uses raw Linux primitives).
> The control plane and `brokk` build on macOS/Windows for development.

## 1. Build

```sh
cargo build --workspace            # or --release for realistic timing
```

Binaries land in `target/debug/` (or `target/release/`): `brokkr-control`,
`brokkr-worker`, `brokk`.

## 2. Start the control plane

```sh
brokkr-control --listen 127.0.0.1:7878 --data-dir ./brokkr-data
```

- `--listen` — gRPC bind address (default `127.0.0.1:7878`).
- `--data-dir` — on-disk CAS + action cache (created if absent).

It logs `TLS DISABLED` and `CLIENT AUTH DISABLED` warnings in this open-mode
dev configuration — see [Security](#5-security-tls--auth) to lock it down.

## 3. Start one or more workers

Each worker registers with the control plane, advertises its `os`/`arch`
capabilities, heartbeats every 5 s, and runs one job at a time.

```sh
# Sandboxed (default; requires the Phase-2 host setup — see --check-host):
brokkr-worker --control http://127.0.0.1:7878

# Development without the sandbox (runs actions as plain host processes):
brokkr-worker --control http://127.0.0.1:7878 --no-sandbox
```

Run the same command again (in another terminal) to add more workers; the
scheduler spreads jobs across them and fair-shares across tenants.

Check a host can run the sandbox before starting:

```sh
brokkr-worker --check-host
```

## 4. Run a job

```sh
brokk run --control http://127.0.0.1:7878 -- /bin/echo "hello cluster"
```

`brokk` uploads the action to the CAS, calls `Execute`, streams the result, and
exits with the action's exit code. A second identical run is served from the
action cache.

## 5. Security (TLS + auth)

Production deployments enable both, and once any JWT auth flag is on the
control plane binds a separate listener for `WorkerService` + the
worker's CAS writes (issue #139 — without the split, the worker would
share the JWT-gated listener and every `batch_update_blobs` would be
rejected with `UNAUTHENTICATED`). The control plane refuses to start in
contradictory combinations and prints a clear error.

### Worked split-port invocation

Generate the certs first (test fixtures at
`crates/brokkr-control/tests/fixtures/` document the shape; for
production use your own CA and a SAN that matches your endpoint host):

```sh
# Terminal 1 — control plane (production posture)
brokkr-control \
  --listen 127.0.0.1:7878 \
  --worker-listen 127.0.0.1:7879 \
  --data-dir ./brokkr-data \
  --tls-cert ./certs/server.pem \
  --tls-key  ./certs/server.key \
  --tls-client-ca ./certs/ca.pem \
  --auth-jwt-hmac-secret-file ./certs/jwt.secret
```

What each listener enforces:

- **`127.0.0.1:7878` (client port)** — TLS + JWT bearer on
  `ContentAddressableStorage` (client surface), `ActionCache`,
  `Capabilities`, `Execution`. A client cert is *optional*; the JWT
  interceptor is the authoritative boundary.
- **`127.0.0.1:7879` (worker port)** — TLS + **mTLS required** on
  `WorkerService` AND the worker-side `ContentAddressableStorage` (so
  workers can upload stdout/stderr). No JWT interceptor; the worker is
  authenticated by the client cert it presents at the transport layer.

```sh
# Terminal 2 — worker
brokkr-worker \
  --control       https://127.0.0.1:7878 \
  --worker-control https://127.0.0.1:7879 \
  --ca            ./certs/ca.pem \
  --client-cert   ./certs/worker.pem \
  --client-key    ./certs/worker.key
```

The worker refuses to start if `--worker-control` is `https://` and no
client cert/key is set (the server's worker port would reject every
TLS handshake).

```sh
# Terminal 3 — client (mint a JWT signed with ./certs/jwt.secret first)
brokk run \
  --control      https://127.0.0.1:7878 \
  --ca           ./certs/ca.pem \
  --bearer-token "$JWT" \
  -- /bin/echo "hello mTLS"
```

Omit `--bearer-token` on the same call and the client port returns
`UNAUTHENTICATED`. Run with the same JWT twice and the second
`Execute` hits the action cache.

### Refuse-to-start combinations

The control plane exits with a clear error before binding a listener:

- `--single-port --auth-jwt-*` — the worker would share the JWT-gated
  listener and `batch_update_blobs` would fail.
- `--auth-jwt-*` (without `--tls-client-ca`) — the worker port cannot
  authenticate anyone.
- `--tls-client-ca` without `--tls-cert`/`--tls-key` — mTLS requires a
  server identity to present.

See `crates/brokkr-control/src/main.rs::Args::validate_auth_flags` and
the integration suite at `crates/brokkr-control/tests/split_port_cluster.rs`.

## 6. Convenience script

`scripts/run-cluster.sh` builds the workspace and starts a control plane plus
one `--no-sandbox` worker locally, for quick experimentation:

```sh
./scripts/run-cluster.sh
# then, in another shell:
brokk run --control http://127.0.0.1:7878 -- /bin/echo hi
```

## 7. Automated two-process tests

The open-mode end-to-end suite:

```sh
cargo build --workspace
cargo test -p brokkr-control --test two_process_cluster -- --ignored --nocapture
```

This spawns the real `brokkr-control` + `brokkr-worker` binaries and runs a job
through `brokk` end-to-end. It is `#[ignore]` by default (it needs the binaries
built and spawns processes).

The split-port mTLS+JWT suite (issue #139 verification):

```sh
cargo test -p brokkr-control --test split_port_cluster -- --ignored --nocapture
```

Seven tests covering the production posture (TLS server cert + mTLS client cert
on the worker port + JWT bearer on the client port), the three refuse-to-start
combinations, and the happy-path `Execute` that round-trips worker stdout/stderr
through the shared CAS.

## HA control plane (Phase 5 I9, experimental)

Three control planes replicating metadata (the action cache; cluster config
to follow) through the from-scratch Raft. Each node needs a distinct
`--node-id`, a `--raft-listen` address for peer traffic, and a `--raft-peer
id=host:port` for every *other* member:

```sh
# Node 0
brokkr-control --listen 127.0.0.1:7878 --data-dir ./node0   --raft --node-id control-0 --raft-listen 127.0.0.1:7978   --raft-peer control-1=127.0.0.1:7979 --raft-peer control-2=127.0.0.1:7980
# Node 1 / node 2: same shape, own ports + ids, peers = the other two.
```

Or, all three at once:

```sh
./scripts/run-cluster.sh --ha
```

Writes are accepted by the **leader**; a follower answers
`FAILED_PRECONDITION` with the leader's id in `x-brokkr-leader` **and its
address in `x-brokkr-leader-addr`** (I9b) — clients built on `brokkr-sdk`
follow that automatically. Reads through the leader are linearizable
(ReadIndex).

### Secure the Raft peer plane (mTLS)

Brokkr has **three** authentication planes, and they authenticate different
things (ADR 0011):

| Plane | Who talks | Authentication |
|---|---|---|
| Client | `brokk`, Bazel, SDK | JWT bearer (+ server TLS) |
| Worker | `brokkr-worker` → control | mTLS, client cert required |
| **Raft peer** | control ↔ control | **mTLS, mutual only — no JWT** |

The peer plane matters most of the three: `AppendEntries` on it **appends to
the replicated log**, so an unauthenticated peer port is a write path into
consensus itself. A peer is not a tenant and has no user identity to carry,
which is why it is mutual-only rather than JWT-gated.

```sh
brokkr-control --listen 127.0.0.1:7878 --data-dir ./node0 \
  --raft --node-id control-0 --advertise-addr 127.0.0.1:7878 \
  --raft-listen 127.0.0.1:7978 \
  --raft-peer control-1=127.0.0.1:7979 --raft-peer control-2=127.0.0.1:7980 \
  --raft-tls-ca ./certs/raft-ca.pem \
  --raft-tls-cert ./certs/control-0.pem --raft-tls-key ./certs/control-0-key.pem
```

Every node needs a certificate the others' `--raft-tls-ca` verifies; the same
flags configure **both** the `--raft-listen` server and the outbound peer
channels, because a one-sided configuration fails at handshake time rather
than at startup. `brokkr-control` **refuses to start** on a half-configured
raft plane (cert without key, TLS on one side only, or a CA that does not
verify the configured cert) — the same posture issue #139 established for the
other two planes.

Plaintext peer links remain available for local development
(`scripts/run-cluster.sh --ha`) and are **dev-only**: on a shared network they
expose the replicated log to anyone who can reach the port.

### Run a worker on every control node

**This is a requirement, not a suggestion.** The worker registry is per-node
and deliberately **not** replicated through Raft (ADRs 0008–0010: workers
re-register and leases reassign on failure, so replicating it would buy
nothing and cost consensus round trips on every heartbeat). The consequence:

> A control node with no worker attached **cannot execute anything.** A build
> routed there fails `FAILED_PRECONDITION: no eligible worker` no matter how
> healthy the Raft cluster is.

So a three-node control plane needs workers on all three. Give each worker
every endpoint — it attaches to the first that answers and rotates to another
when that node dies:

```sh
brokkr-worker --control http://127.0.0.1:7878 \
              --control http://127.0.0.1:7879 \
              --control http://127.0.0.1:7880
```

### What a follower-routed build does

Under decision D1 a build that lands on a follower **succeeds but is not
cached**: the follower can neither read nor write the replicated action cache,
and a completed action is too expensive to discard over a routing accident.
`brokk` prints the reason, and the control plane counts these
(`uncached_results_not_leader`). A steadily climbing count means clients are
being served by followers — the builds are correct, but every one of them is
paying full execution cost. Route clients at the leader, or accept the cost
deliberately.

### The real-process tests

```sh
cargo build --workspace
# DoD 1: kill the leader, a survivor accepts writes in under two seconds
cargo test -p brokkr-control --test raft_ha_cluster -- --ignored --nocapture
# The full story: 3 control nodes + 3 workers + `brokk run` across a leader kill
cargo test -p brokkr-control --test raft_ha_e2e -- --ignored --nocapture
```

## Known gap

A multi-*node* (multi-host) deployment and the REAPI Bazel-compatibility test
(`bazel build` against `brokk` as the remote executor) are not yet covered —
see the Phase 4 wrap-up in `docs/journal/phase-4.md`.
