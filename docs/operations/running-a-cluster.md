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

Production deployments should enable both:

- **TLS / worker mTLS:** `--tls-cert`, `--tls-key`, and `--tls-client-ca` on
  `brokkr-control` (and the matching `--ca-cert` / client cert flags on the
  worker). With `--tls-client-ca` set, the control plane requires worker client
  certificates.
- **Client JWT auth:** `--auth-jwt-hmac-secret-file` *or*
  `--auth-jwt-rsa-pem-file` (plus optional `--auth-jwt-issuer` /
  `--auth-jwt-audience` / `--auth-jwt-tenant-claim`). Clients then send
  `authorization: Bearer <jwt>`; the token's tenant claim is authoritative. See
  ADR 0011.

## 6. Convenience script

`scripts/run-cluster.sh` builds the workspace and starts a control plane plus
one `--no-sandbox` worker locally, for quick experimentation:

```sh
./scripts/run-cluster.sh
# then, in another shell:
brokk run --control http://127.0.0.1:7878 -- /bin/echo hi
```

## 7. Automated two-process test

```sh
cargo build --workspace
cargo test -p brokkr-control --test two_process_cluster -- --ignored --nocapture
```

This spawns the real `brokkr-control` + `brokkr-worker` binaries and runs a job
through `brokk` end-to-end. It is `#[ignore]` by default (it needs the binaries
built and spawns processes).

## Known gap

A multi-*node* (multi-host) deployment and the REAPI Bazel-compatibility test
(`bazel build` against `brokk` as the remote executor) are not yet covered —
see the Phase 4 wrap-up in `docs/journal/phase-4.md`.
