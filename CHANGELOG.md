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
