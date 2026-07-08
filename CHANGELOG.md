# Changelog

All notable changes to Brokkr will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Security
- Reviewed the seccomp `socket` / `socketpair` allowance (issue #69) and
  replaced the terse "netns blocks egress" comment with the full
  rationale: the action's network namespace scopes abstract `AF_UNIX`
  sockets (and blocks IP), the mount namespace hides host pathname
  sockets, and `socketpair` is intra-process — so `SCM_RIGHTS` cannot
  smuggle fds across the sandbox boundary. No behavioural change;
  `socketpair` stays allowed because removing it would break legitimate
  in-sandbox IPC for no security gain.

### Fixed
- `brokkr-control` REAPI services (`ContentAddressableStorage`,
  `ActionCache`, `Execution`, `Capabilities`) now reject any request
  carrying a non-empty `instance_name` with `INVALID_ARGUMENT` instead
  of silently serving it from the single default instance. Phase 1 has
  no multi-tenant isolation, so accepting a caller-named instance was a
  latent cross-instance access gap (issue #72). The SDK always sends an
  empty `instance_name`, so this is transparent to existing clients.
- `brokkr-sandbox` host runner no longer swallows a panicked or
  cancelled stdout/stderr capture task. `JoinError` was discarded by
  `unwrap_or_default()`, so a crashed pump returned empty output with
  no signal to the operator (issue #68). The join is now handled
  explicitly: on error it logs a `warn` naming the affected stream and
  falls back to an empty buffer.
- `brokkr-sdk` no longer discards the `google.rpc.Status.code` on a
  failed execution. The streamed `Operation` error path flattened the
  code into an `anyhow!` string, and the `ExecuteResponse.status` path
  stringified the structured `ExecuteError` — so callers could not tell
  `RESOURCE_EXHAUSTED` / `UNAVAILABLE` / `DEADLINE_EXCEEDED` apart for
  retry decisions (issue #62). Both paths now propagate the structured
  `ExecuteError::Status { code, message }`, recoverable by downcasting
  the returned `anyhow::Error`.
- `brokkr-control` CAS service now bounds batch request sizes at the
  service boundary. `FindMissingBlobs`, `BatchReadBlobs`, and
  `BatchUpdateBlobs` previously iterated over an unbounded request, so a
  client could exhaust control-plane memory with one giant batch (issue
  #66). A new `BatchLimits` config (default 1000 blobs/request, 4 MiB
  payload — matching the advertised `max_batch_total_size_bytes`) is
  enforced before allocation; oversized requests are rejected with
  `INVALID_ARGUMENT`. Construct with `CasService::with_limits` to
  override.
- `brokkr-worker` and `brokkr-sandbox` now bound how much of an action's
  stdout/stderr they buffer. The worker's plain runner used
  `Command::output()` and the sandbox host drained the runner pipes with
  an unbounded `read_to_end`, so an action writing gigabytes to stdout
  could OOM the worker (issue #67). Both paths now cap each stream at
  50 MiB (`read_capped`), draining and dropping the excess with a `warn`
  rather than buffering it.
- `brokkr-sandbox::runner::seccomp` compiled with a stray `return`
  inside a single-arm `cfg` block which a newer clippy flagged as
  `needless_return`; replaced with bare expressions so all four
  arch arms keep the same shape.
- `brokkr-sandbox::runner::seccomp::syscall_nr` referenced
  `libc::SYS_fadvise64`, which `libc` does not expose on aarch64
  Linux (the kernel calls the syscall `arm64_fadvise64_64`
  internally and `libc` has no constant for it). The lookup is
  now gated to `x86_64` / `riscv64`; on aarch64 the `fadvise64`
  allowlist entry is silently dropped, which is the same fall-back
  `syscall_nr` already does for arch-absent entries. Restores
  `cargo test` on the aarch64-unknown-linux-gnu CI matrix entry.
- `brokkr-control::worker_service::stream` inbound pump no longer
  exits silently when the worker stream returns `Ok(None)` (clean
  close), `Err` (transport / decode failure), or a message with no
  payload. Each terminal state now logs at the appropriate level
  (`info` / `error` / `warn`) so an operator can tell why the pump
  stopped (issue #64).
- `brokkr-control::Scheduler::execute` no longer waits forever for a
  worker result. The oneshot wait is wrapped in `tokio::time::timeout`
  honouring REAPI `Action.timeout` per-action and falling back to a
  scheduler-wide default (30 minutes, override via
  `Scheduler::with_execution_timeout`). On expiry the waiter slot is
  reclaimed and the call returns the new typed
  `ExecutionError::Timeout(Duration)`, which the `Execution` service
  surfaces to clients as gRPC `DEADLINE_EXCEEDED` (code 4) so they
  can retry without parsing error strings (issue #63).
- `brokkr-control` CAS service now verifies each `BatchUpdateBlobs`
  entry's declared digest against its payload *at the service
  boundary* and rejects mismatches with per-entry
  `INVALID_ARGUMENT`. The backend has always re-verified before
  storing (so no data corruption was ever possible) — the
  service-layer check just avoids a `spawn_blocking` redb txn for
  known-bad entries and gives the client an earlier, cheaper
  rejection (issue #70).

### Changed
- `Scheduler::execute` now returns `Result<ExecutionOutcome,
  ExecutionError>` (was `anyhow::Result<ExecutionOutcome>`); the
  enum has `Timeout(Duration)` and `Other(anyhow::Error)` variants.

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
- `brokkr-sandbox`: seccomp argument-level filtering for `prctl` and `ioctl`
  (blocks `PR_SET_KEEPCAPS`, `PR_CAPBSET_DROP`, `PR_SET_TSC`, `PR_GET_TSC`,
  and terminal/device `ioctl` calls: `TIOCSTI`, `TIOCSWINSZ`, `TIOCGWINSZ`,
  `TIOCSBRK`, `TIOCCBRK`, `TIOCSPTLCK`). `SYS_fadvise64` removed from the
  syscall allowlist (absent on aarch64). Tests for
  `ev09_prctl_set_tsc_blocked`, `ev_prctl_keepcaps_blocked`,
  `ev_prctl_capbset_drop_blocked`, `ev_prctl_get_tsc_blocked`,
  `ev_ioctl_tiocsti_blocked`, `ev_ioctl_tiocgwinsz_blocked`,
  `ev_ioctl_tiocswinsz_blocked`, and `ev_ioctl_tiocptlck_blocked`.

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
- M5: network namespace + optional loopback. `unshare` now also asks
  for `CLONE_NEWNET`, so the action lands in an empty netns by
  default — no interfaces, no routes, not even loopback. With
  `NetworkPolicy::Loopback`, the runner sends a hand-rolled
  `RTM_NEWLINK` over a raw netlink socket to flip `lo` `UP` before
  the action exec. New `runner/netns.rs` module; ~150 lines, no
  extra crate dependencies.
- `brokkr-sandbox/tests/net_ns.rs` — three M5 tests using `python3`
  to read `errno` through the action's exit code: EV-08 (`1.1.1.1:443`
  is `ENETUNREACH`), policy-`None` makes `127.0.0.1` `ENETUNREACH`
  too (lo is `DOWN`), and policy-`Loopback` upgrades `127.0.0.1`'s
  failure mode from `ENETUNREACH` to `ECONNREFUSED` — proving the
  link is actually up.
- M6: per-action cgroup-v2 + wall-clock timeout + OOM detection +
  resource accounting. `Sandbox::with_cgroup_root(path)` opts into
  it; without that builder call the sandbox does no cgroup work and
  M2-M5 behaviour is preserved. New host module `host/cgroup.rs`
  creates `<root>/action-<uuid>/`, applies `memory.max` /
  `memory.swap.max` / `pids.max` / `cpu.max` from
  `ResourceLimits`, attaches the runner pid to `cgroup.procs`
  before the runner makes any progress, reads `cpu.stat` /
  `memory.peak` / `pids.peak` / `io.stat` for accounting, and
  checks `memory.events:oom_kill` to translate kernel-OOM kills into
  `ExitStatus::OutOfMemory`. `cgroup.kill` is used for atomic
  cleanup on wall-clock timeout (kernel ≥ 5.14, with a per-pid
  fallback).
- Host-side wall-clock enforcement: when `ResourceLimits.wall_clock_secs`
  is set, the runner wait is wrapped in `tokio::time::timeout`; on
  elapsed the cgroup is `kill_all`'d (or the runner alone, sans
  cgroup) and `ExitStatus::Timeout` is returned.
- `host/linux.rs` now drives stdout/stderr drains via
  `tokio::spawn(read_to_end)` rather than `wait_with_output`, so
  `child.wait()` is interruptible by the timeout path.
- `brokkr-sandbox/tests/cgroup.rs` — four M6 tests:
  `wall_clock_timeout_kills_long_action` (always runs),
  `fork_bomb_capped_by_pids_max`, `memory_max_triggers_oom_status`,
  `accounting_is_populated_for_a_normal_action`. The last three
  skip cleanly unless `BROKKR_TEST_CGROUP_ROOT` is set or
  `/sys/fs/cgroup/brokkr.slice/` is writable; locally they pass under
  `systemd-run --user --scope -- bash -c 'BROKKR_TEST_CGROUP_ROOT=…
  cargo test'`.
- Added `uuid` as a direct dep of `brokkr-sandbox` for action-cgroup
  naming.
- M7 (partial): default-deny seccomp-bpf filter in
  `brokkr-sandbox/src/runner/seccomp.rs`. Compiles a
  `seccompiler::SeccompFilter` whose mismatch action is
  `Errno(EPERM)` and whose match action is `Allow`, with the syscall
  allowlist from `docs/phase-2-plan.md` §5.6 plus an additive
  `extra_seccomp_allow` slot. Syscall names are resolved to numbers via
  `nix::libc::SYS_*` (seccompiler 0.5 does not expose its internal
  name table publicly); names absent on the current arch are silently
  skipped from the default list, but unknown names in the extra list
  are rejected with `InvalidInput`. Argument-level filtering for
  `prctl`/`ioctl` is intentionally deferred (TODO marker in the
  source). Wiring `install()` into the runner pipeline is the next
  M7 step and is owned by the integration agent.
- M7 (capabilities): `runner/caps.rs` drains the Bounding /
  Permitted / Effective / Inheritable / Ambient capability sets
  down to `SandboxConfig.retained_caps` (default empty) and sets
  `PR_SET_NO_NEW_PRIVS=1` so subsequent `execve` cannot regain
  privileges via setuid binaries or file caps. Capset order is
  Effective → Permitted → Inheritable to satisfy the kernel's
  "new Effective ⊆ new Permitted" invariant. Ambient is
  best-effort (older kernels return ENOENT).
- M7 (integration): `runner/exec.rs` calls
  `caps::drop_all_except` followed by `seccomp::install` in the
  action child, after `chdir(workdir)` and immediately before
  `execve`. The hardening only fires on the namespace path
  (non-empty `RootfsSpec`) — the M2 no-isolation path stays as
  a thin host-process wrapper since it lacks `CAP_SETPCAP` to
  drain the bounding set.
- M7 (seccomp allowlist): expanded `DEFAULT_ALLOW` to cover the
  common workload syscall surface — directory iteration
  (`getdents`/`getdents64`), file metadata (`access`,
  `readlink`/`readlinkat`, `statfs`/`fstatfs`), file mutation
  (`mkdir`/`mkdirat`, `unlink`/`unlinkat`, `rename` family,
  `chmod`/`chown` family, link/symlink family, `truncate`,
  `fsync`/`fdatasync`, `utimensat`, `umask`), TCP/IP socket I/O
  (`connect`/`bind`/`listen`/`accept[4]`/`shutdown`,
  `getsockname`/`getpeername`, `setsockopt`/`getsockopt`,
  `sendto`/`recvfrom`, `sendmsg`/`recvmsg`, `sendmmsg`/`recvmmsg`),
  signal delivery (`kill`/`tkill`/`tgkill`), and a handful of
  modern glibc helpers (`rseq`, `membarrier`, `set_tid_address`,
  `fadvise64`). `mount`, `keyctl`, `ptrace`, `bpf`, `userfaultfd`,
  `init_module` etc. remain blocked → EPERM.
- `brokkr-sandbox/tests/evil_seccomp_caps.rs` — five new M7 evil
  tests (EV-02 mount, EV-03 keyctl, EV-04 ptrace, EV-10
  no_new_privs, EV-14 CapEff zeroed) plus an `#[ignore]`d EV-09
  RDTSC stub (TODO: needs `prctl(PR_SET_TSC)` wiring + arg-level
  prctl filter, both deferred). Tests skip cleanly when the host
  cannot open an unprivileged user namespace, mirroring
  `mount_ns.rs` / `net_ns.rs`.
- M8 (determinism): `runner/determinism.rs` applies the
  hostname/symlink half pre-fork (after the rootfs is built and
  inside the new UTS namespace) and scrubs the action's env
  pre-exec. `apply_pre_fork` calls `sethostname(2)` (when
  `DeterminismPolicy.hostname` is set) and symlinks
  `/etc/localtime` → `/usr/share/zoneinfo/Etc/UTC` when
  `timezone_utc` is set; `scrub_env` filters `LD_PRELOAD` /
  `LD_LIBRARY_PATH` (`strip_ld_preload`), replaces `PATH` with
  `/usr/bin:/bin` (`strip_path`), and upserts `TZ=UTC0` /
  `SOURCE_DATE_EPOCH` when their knobs are set.
- M8: `unshare` flags extended with `CLONE_NEWUTS` so
  `sethostname(2)` only affects the sandbox; required adding the
  `hostname` feature to the workspace `nix` dependency.
- M8: `DeterminismPolicy::brokkr_defaults()` — the policy the
  worker applies by default (hostname `brokkr-sandbox`, UTC
  timezone, LD_PRELOAD stripped, PATH replaced; SOURCE_DATE_EPOCH
  left for per-action override).
- `brokkr-sandbox/tests/determinism.rs` — five M8 tests:
  hostname is the configured value, EV-11 (`LD_PRELOAD` stripped
  before the action sees the env), `TZ` set to `UTC0`,
  `SOURCE_DATE_EPOCH` injected, and AC-04 (two identical runs
  produce byte-identical stdout under `brokkr_defaults`). Same
  unprivileged-userns skip policy as the other namespace-path
  evil tests.
- M9: `brokkr-worker` runs actions through the sandbox by default.
  New `Runner` enum (`Plain` vs `Sandboxed(Box<SandboxRunner>)`)
  in `brokkr-worker::runner`; `SandboxTemplate::brokkr_default()`
  encodes the worker's defaults (minimal usrmerge rootfs, no
  network, `DeterminismPolicy::brokkr_defaults()` for hostname /
  TZ / env scrubbing, no resource limits). `run_command` accepts
  `&Runner` and dispatches; signal-kill / OOM / wall-clock are
  mapped to shell-convention exit codes (`128+sig`, `137`, `124`)
  so the existing `ActionResult.exit_code` plumbing carries the
  information.
- M9: `WorkerConfig.runner` field. Default is `Runner::Plain` so
  in-process test fixtures don't need the `brokkr-sandboxd`
  binary; the CLI `brokkr-worker` builds a `Runner::Sandboxed`
  unless `--no-sandbox` is passed.
- M9: `brokkr-worker` CLI flags — `--no-sandbox` (Phase 1
  fallback, logs a warning), `--sandbox-runner PATH`,
  `--sandbox-cgroup-root PATH`, `--sandbox-wall-clock-secs N`,
  `--sandbox-memory-bytes N`, `--sandbox-pids-max N`. The default
  sandboxed mode probes the host (`host_check::run`) at startup
  and refuses to start if the host can't run the sandbox; the
  error message points at `--check-host` and `--no-sandbox`.
- `brokkr-worker/tests/sandbox_e2e.rs` — four M9 tests:
  `/bin/echo "hello world"` round-trips through the sandbox
  runner; hostname inside is `brokkr-sandbox`; `/etc/shadow` is
  not visible (rootfs allowlist works); the `Plain` runner is
  still Phase-1 compatible. Resolves `brokkr-sandboxd` via
  `current_exe()` since `CARGO_BIN_EXE_*` only sets for the
  owning crate's tests. Same unprivileged-userns skip macro as
  the other namespace-path tests.

## Phase 3 (in progress)

### M7 — three-node soak + Phase 3 wrap-up

- M7: `crates/brokkr-cas/tests/three_node_soak.rs` — Phase 3's
  release-gate soak test, per `docs/phase-3-plan.md` §7.3 +
  §7.3.1. Drives a three-node `ReplicatedCas` (R=2) over a
  `MutablePool` of `InMemoryCas` backends through a
  put/get/find_missing mix, with one node restarted (swapped
  for a fresh empty backend + `repair_node` to convergence)
  every `BROKKR_SOAK_CHURN` ops. End-of-run asserts the four
  §7.3.1 invariants: no data loss (byte-level readback of
  every put), no orphans (final `repair_cluster` reports zero
  repairs / zero unrepairable), quiescence (< 1 s for the
  final cluster scan), and bounded per-node count
  (`list_digests` matches HRW assignment per node).
  `#[ignore]` by default. Default budget (25k ops / ~99
  churns) runs in ~28 s on a workstation; release-gate
  budget (`BROKKR_SOAK_OPS=1000000`, etc.) wired through env
  vars for CI.
- M7: `docs/phase-3-plan.md` §7.3.1 sub-plan added (backend
  choice, default budget, op mix, churn loop, invariants,
  out-of-scope list).
- M7: `docs/journal/phase-3.md` — M7 retrospective + Phase 3
  wrap-up (`### Phase 3 wrap-up (M0–M7)`) covering DoD
  status, deferred items routed to Phase 4, and rough
  Phase 3 numbers.
- M7: new `brokkr-cas` dev-deps `rand = "0.8"` and
  `parking_lot = "0.12"`. Both already transitively in the
  workspace lockfile via existing crates; no new external
  surface added to non-test builds.


### Added
- `docs/phase-3-plan.md` — detailed Phase 3 implementation plan
  (failure model, rendezvous-hashing routing, tiered storage
  composition, replication strategy, FUSE materialisation,
  garbage-collection sketch, M0–M7 milestones, CI / host
  compatibility notes, deferred items).
- `docs/journal/phase-3.md` — Phase 3 journal, started with the M0
  entry. Records the architecture decisions made before any code
  lands (HRW over consistent hashing, push-based membership stream,
  `R/2+1` write quorum + async repair).
- M1: `brokkr.v1.membership.proto` — `CasNode`, `NodeStatus`,
  `TopologyView`, and `MembershipService.WatchTopology` (long-lived
  server-streaming RPC). The existing `brokkr.v1` module gains new
  types; the build script picks up the new proto.
- M1: `brokkr-cas::ring` — rendezvous-hash (HRW) replica picker.
  Pure-data `RingNode` + `NodeStatus`; `replicas_for(digest, nodes,
  R)` returns the top-R eligible nodes ordered primary-first.
  Backed by sha256. Seven unit tests, including distribution
  uniformity (10k digests over 4 nodes, ±10% per node) and
  one-node-removal churn (~20% in expectation, asserted in the
  10–35% band on a 4k-digest sample).
- M1: `brokkr-cas::router::Router` — client-side topology view
  built on top of `ring`. Atomically swappable via
  `update_topology`; `primary_replicas_for(&digest)` consults the
  current view at the configured replication factor. `RwLock`-based
  read path; lock poisoning recovery is graceful. Four unit tests.
- M1: `brokkr-control::membership::Membership` +
  `MembershipServiceImpl` — `Membership` holds a
  `watch::Sender<TopologyView>` that every subscribed client tails;
  `set_nodes` / `set_replication_factor` bump the generation iff
  the effective view changes (`send_if_modified`).
  `MembershipServiceImpl` adapts the watch receiver into a tonic
  server-stream. Five unit tests.
- M1: `brokkr-control/tests/membership.rs` — two integration tests.
  `watch_topology_streams_current_view_then_updates` exercises the
  gRPC flow end-to-end: first message on connect is the current
  view, subsequent messages arrive on `set_nodes`.
  `router_routes_consistently_with_local_topology` proves the
  proto round-trip is lossless: a `Router` fed from the gRPC
  stream picks the same primary for each digest as one built
  directly from a hand-rolled `Topology`.
- `tokio-stream` gains the `sync` feature on `brokkr-control` for
  `WatchStream`.
- M2: `brokkr-cas::bloom::Bloom` — hand-rolled bloom filter sized
  from `(expected_items, fp_rate)` via the standard formulas
  (`m = ⌈-n·ln p / (ln 2)²⌉`, `k = ⌈(m/n)·ln 2⌉`). Hashes are
  derived from the digest's existing sha256 via the
  Kirsch–Mitzenmacher construction (`h_i = h1 + i·h2` mod m), so
  insert/check is plain bit-ops without re-hashing. Seven unit
  tests including an empirical false-positive-rate check (10k
  members at p=0.01, 100k probes, rate < 2% asserted) and a
  sizing-formula spot check (1M @ 1% → ~9.6 Mbits, k=7).
- M2: `brokkr-cas::BloomCas<C: Cas>` — decorator wrapping any
  `Cas` backend. `find_missing_blobs` partitions inputs into
  bloom-says-missing vs. bloom-says-maybe; only the latter
  consults the underlying backend. `batch_update_blobs`
  delegates and inserts into the bloom on success.
  `batch_read_blobs` delegates unchanged (the bloom doesn't help
  reads). `rebuild_from` reseeds the filter from an authoritative
  digest source for the periodic-rebuild path. Six tests.
- No new direct deps — bloom is built from the existing
  `sha2` and standard library.
- M3a: `brokkr-cas::tiered::TieredCas<W: Cas>` — composes an
  in-memory size-bounded LRU "hot" tier in front of any `Cas`
  backend (typically `RedbCas`). Reads serve from hot on hit;
  hot misses fall through to warm and promote on success. Writes
  populate warm authoritatively and hot eagerly (workers
  frequently re-read what they just wrote). `find_missing_blobs`
  delegates straight to warm — hot is a cache, not authoritative.
- M3a: Hand-rolled `HotTier` LRU. Byte-bounded (not entry-bounded
  — blob sizes vary too widely for a count to be useful). Hash
  map for O(1) lookups + intrusive doubly-linked-list of `usize`
  indices into a node pool, with a free-list for evicted slots.
  All safe Rust. Blobs larger than the whole capacity are not
  cached (avoids evicting the entire tier for one outsized
  insert). Zero capacity disables hot caching entirely (useful
  for tests).
- M3a: Eleven unit tests on `HotTier` + `TieredCas` covering
  empty get, put/get round-trip, LRU eviction, MRU touch,
  oversized-blob skip, zero-capacity bypass, hot warmup on
  write, warm-to-hot promotion on read, find-missing delegation,
  and rejection of failed writes from the cache.
- Cold tier (OpenDAL S3) is deferred to M3b — a follow-up PR.
- M4: `brokkr-cas::replicated::ReplicatedCas<P: ReplicaPool>` —
  quorum-write + read-fan-out across the `R` replicas selected
  by the rendezvous ring. Writes succeed at `⌈R/2⌉ + 1` acks;
  reads try replicas primary-first and return the first
  success. `find_missing_blobs` queries the primary (with
  failover to the next replica if the primary is unreachable).
- M4: `ReplicaPool` trait + `StaticPool` impl. The pool maps
  `node_id → Arc<dyn Cas>`; a future milestone will provide a
  gRPC-backed pool, but M4 ships the logic and tests in-process
  against pools of `InMemoryCas` instances.
- M4: Seven `ReplicatedCas` tests — write fan-out lands on
  exactly R replicas, read serves from first-available, read
  returns NotFound when no replica has the blob, quorum holds
  on one-replica-down with R=3, quorum fails when only 1/2
  reachable, find-missing returns authoritative answer, empty
  topology fails closed.
- `futures` workspace dep added to `brokkr-cas` for
  `future::join_all` on the replica fan-out.
- M5a: `brokkr-cas::gc` — reference-counted GC for the CAS.
  `plan(&cas, &action_cache)` walks every cached `ActionResult`,
  extracts the digests inlined directly (output_files,
  stdout/stderr, output_directories' tree / root digests), and
  returns the set difference `local_digests − reachable` as the
  candidate-deletion list. `sweep` runs `plan` and deletes every
  unreachable digest; `sweep_with_plan` lets callers dry-run +
  apply a custom retention filter between the two steps.
- M5a: `Cas` trait gains `list_digests` and `delete_blob`
  default-implemented as empty / no-op so non-GC test backends
  still compile. `InMemoryCas` and `RedbCas` both override.
  `ActionCache` trait gains `list_entries` (same pattern);
  `RedbActionCache` overrides.
- M5a: six unit tests on `gc` — direct-digest extraction (output
  + stdout + stderr), malformed-proto skip, plan correctness on
  a 2-blob CAS with one cached output, sweep deletes the right
  set, empty action cache → everything unreachable, sweep
  idempotency.
- Transitive reachability (walking `Directory` Merkle DAGs) is
  deferred to a later milestone — that walk requires CAS reads
  and wants its own scheduling. M5a's non-transitive
  reachability covers output files (the bulk of blob volume).
- Retention window + atime tracking deferred to M5b; M5a deletes
  unreachable blobs immediately.
- M5b: `brokkr-cas::peer::repair_node(pool, topology, target)` —
  reconciles one target node's local digest set with what HRW
  says it should hold. Scans the universe of digests across all
  reachable replicas, computes HRW assignment for each, pulls
  bytes from peers for any the target is missing. Returns a
  `RepairReport` summarising `expected` / `already_present` /
  `repaired` / `unrepairable`. `repair_cluster` runs
  `repair_node` against every node.
- M5b: Five unit tests on `peer` — no-op when cluster is
  consistent, restore-a-lost-blob, target-doesn't-get-blobs-it-
  shouldn't-hold (HRW-aware), every-replica-lost-it edge case,
  and `repair_cluster` idempotency.
- M5b: Repair is built on top of M4's `ReplicaPool` abstraction;
  in-process tests use `StaticPool<InMemoryCas>`. A future
  milestone will wrap a gRPC pool (`CasPeer` clients) for
  cross-process repair. The daemon loop / scheduler is also
  deferred — `repair_node` is a one-shot primitive.
- M6a: `brokkr-cas::tree::materialize_tree(cas, root_digest,
  target_dir)` walks a REAPI Directory Merkle DAG and writes a
  faithful copy of the input tree to disk. Files are fetched
  from CAS lazily during the walk; symlinks become real
  symlinks; the Unix executable bit is honoured. Returns
  `MaterializationStats { files, dirs, symlinks, bytes }`.
- M6a: `build_tree_into(cas, source_dir)` — symmetric helper
  that packs a local directory tree into CAS (one `Directory`
  per actual directory, one blob per actual file) and returns
  the root digest. Used by the round-trip tests and useful for
  workers that want to upload their workspace.
- M6a: six unit tests cover empty trees, flat files, nested
  trees, executable-bit preservation, symlink preservation, and
  NotFound propagation on a bogus root digest.
- M6a: `CasError::Other(String)` variant for non-`Io`/non-`Redb`
  failures (proto decode, malformed tree entries). The tree
  module raises it on encode/decode errors.
- FUSE-based lazy materialisation deferred to M6b. M6a is the
  pre-FUSE foundation: workers can use it today on Phase 3
  clusters; M6b will replace it with a FUSE filesystem so trees
  bigger than RAM mount in ~ms without copying every byte.
- M6b sub-plan written into `docs/phase-3-plan.md` §5.5.1 and
  `docs/journal/phase-3.md`. Locks in crate placement
  (`brokkr-worker/src/fuse.rs`), the tokio↔fuser bridge
  (dedicated thread + `Handle::block_on` with a per-mount
  `Semaphore`), the RAII `InputMount` lifecycle, the
  `fuse_device` host probe surfaced via `worker --check-host`,
  and the M6b definition-of-done.
- M6b: `brokkr-worker::fuse` — FUSE-backed lazy input
  materialisation, Linux-only. `fuse::inode::InodeTable`
  builds a path-resolvable inode map from a REAPI `Directory`
  Merkle DAG (one CAS read per directory proto, zero file
  fetches). `fuse::mount::mount(cas, InputMountSpec)` spawns
  a `fuser` background session that serves `lookup` /
  `getattr` / `readdir` / `readlink` from the table and
  fetches file content lazily on first `read(2)`. Per-mount
  `Semaphore(16)` caps concurrent CAS fetches; per-inode
  `tokio::sync::OnceCell<Arc<Mmap>>` coalesces concurrent
  reads of the same file into a single fetch + `mmap`. The
  `InputMount` RAII handle unmounts on drop with a 5 s
  `umount_and_join` timeout and a `fusermount -uz` fallback;
  the per-action cache directory is rm-rf'd after the
  unmount completes.
- M6b: `/dev/fuse accessible` probe added to
  `brokkr-sandbox::checks::linux` and surfaced through the
  existing `brokkr-worker --check-host`. Three outcomes
  (`Pass` / `Warn` for present-but-not-rw / `Fail` for
  missing); does not break sandbox functionality when a
  worker won't use FUSE.
- M6b: seven inode-table unit tests (empty / flat / nested /
  exec-bit / symlink / NotFound / non-dir-parent) and one
  integration test (`tests/fuse_lazy_fetch.rs`,
  `#[ignore]` by default) that mounts a 3-file CAS-backed
  tree, reads two of the three files, and asserts exactly
  two file-content fetches reached CAS.
- M6b: new workspace deps `fuser = "0.17"` and `memmap2 = "0.9"`,
  scoped to `cfg(target_os = "linux")` on `brokkr-worker`.
  Non-Linux hosts get a compile-time stub
  (`fuse::mount::MountError::Unsupported`) so the CLI/SDK
  build stays portable.

## Phase 4 (in progress)

### Added
- `brokkr-control::registry` — in-memory `WorkerRegistry` (plan §16
  task 1). Tracks each worker's `WorkerCapabilities` (hostname +
  free-form labels, mirroring `RegisterWorkerRequest`) and its
  `last_seen` instant; `register` / `record_heartbeat` /
  `evict_stale` / `healthy` operate against a caller-supplied
  `now: Instant` so liveness is deterministic under test (no
  internal `Instant::now`/`SystemTime::now`). `HeartbeatPolicy`
  defaults to the plan's 5 s interval × 3 missed = 15 s eviction
  deadline; `record_heartbeat` on an unknown worker returns the
  typed `RegistryError::UnknownWorker`. Ten unit tests. The
  heartbeat RPC + scheduler worker-selection wiring are the next
  increments; this lands the transport-agnostic data model first.
- `brokkr-control::WorkerServiceImpl` now persists registrations into
  a shared `WorkerRegistry` (`SharedWorkerRegistry =
  Arc<Mutex<WorkerRegistry>>`): `register` records the worker's
  hostname + labels as `WorkerCapabilities` (previously discarded) and
  advertises `heartbeat_seconds` derived from the registry's policy
  interval (5 s) instead of a hardcoded 30, so a worker that honours
  the cadence is never evicted while healthy. New `with_registry`
  constructor + `registry()` accessor share the handle with the
  (forthcoming) heartbeat RPC and eviction tick. `register` gains a
  `#[tracing::instrument]` span recording the assigned `worker_id`.
  Two handler tests (capabilities persisted; distinct id per
  registration).
- `brokkr.v1.WorkerService.Heartbeat` RPC (`HeartbeatRequest{worker_id}`
  → `HeartbeatResponse{known}`). `WorkerServiceImpl::heartbeat` refreshes
  the worker's `last_seen` via `WorkerRegistry::record_heartbeat`; an
  unknown/evicted worker is **not** an error — it gets `known=false` so
  it re-registers instead of retrying a dead identity. A missing
  `worker_id` is `INVALID_ARGUMENT`. Three handler tests (known after
  register, unknown → not known, missing id → invalid argument). The
  worker-side heartbeat sender and the background eviction tick are the
  next increment.
- `brokkr-control::spawn_eviction_task` — background liveness reaper that
  sweeps the shared `WorkerRegistry` once per heartbeat interval and
  evicts workers past the deadline (`interval * max_missed`). Wired into
  the `brokkr-control` binary; a zero interval disables it rather than
  panicking. The eviction decision is `WorkerRegistry::evict_stale`
  (unit-tested with an injected clock), so the wrapper is just the
  periodic driver. New deterministic test
  `eviction_is_observable_via_heartbeat`: register → evict → a
  subsequent `Heartbeat` reports `known=false`.
- `brokkr-worker`: the worker now runs a background heartbeat loop,
  pinging `WorkerService.Heartbeat` on the `heartbeat_seconds` cadence
  the control plane advertised at registration; on `known=false` it logs
  and stops (full re-register is `TODO(brokkr-410)`). Closes plan §16
  task 1 (worker registry + capabilities + heartbeat eviction).
- `brokkr-control::matching` — platform constraint matching (plan §16
  task 2). `labels_satisfy_platform` / `worker_satisfies` implement REAPI
  semantics: a worker satisfies a `Platform` iff it advertises every
  required `Property{name,value}` (empty platform matches everyone).
  `eligible_workers(registry, now, platform)` yields the live workers
  (via `WorkerRegistry::healthy`) that also satisfy the constraints —
  the candidate set the scheduler will dispatch to. Kept proto-aware and
  separate from the proto-free `registry` module (mirrors the Phase 3
  `ring`/proto decoupling). Hard-constraint matching only; soft/preferred
  constraints need a Brokkr convention (future ADR) since REAPI's
  `Platform` has no soft notion. Six unit tests.
- `brokkr-control::Scheduler` platform-constraint **admission control**:
  `execute` now rejects an action up front with the new typed
  `ExecutionError::NoEligibleWorker` (gRPC `FAILED_PRECONDITION`, code 9)
  when no live worker satisfies the action's `Platform`
  (`Action.platform`, falling back to the deprecated `Command.platform`),
  instead of enqueuing a job no worker can claim (which would only
  surface as a timeout). New constructors `Scheduler::with_worker_registry`
  / `with_registry_and_timeout` thread in the shared `WorkerRegistry`;
  the existing `new` / `with_execution_timeout` keep admission control off
  (registry `None`), preserving prior behaviour for fixtures. The
  `brokkr-control` binary now builds one shared registry feeding the
  scheduler (reads), the worker service (writes), and the eviction reaper.
  Three scheduler tests (reject on no worker, reject on label mismatch,
  pass-through with a matching worker).
- `brokkr-worker` now advertises platform capabilities at registration:
  `os` and `arch` labels from the build target (`std::env::consts`), so
  the control plane's constraint matcher can actually place
  platform-constrained actions on it. Completes the §16 task 2
  constraint-matching path end-to-end (worker advertises → matcher →
  admission control). Richer / configurable capabilities (installed
  tools, GPU, RAM) are a later increment. One unit test.
- ADR 0008 — multi-worker scheduling: per-worker job queues with
  submit-time routing + a pluggable selection `Strategy` (chosen over a
  central dispatcher / pull model). `docs/architecture/0008-multi-worker-scheduling.md`.
- `brokkr-control::scheduling` (plan §16 task 3 foundation): the
  `Strategy` trait + `LoadView` and a `SimpleFifo` strategy
  (least-loaded candidate, deterministic id tie-break), plus
  `ConnectedWorkers` — a registry of workers with a live stream, each
  with its own job channel and in-flight count (distinct from
  `WorkerRegistry`, which tracks liveness/capabilities). Pure
  data-model + policy; the scheduler/worker-service wiring that routes
  jobs through it is the next increment. Eight unit tests.
- Multi-worker dispatch wired through the scheduler (ADR 0008). The
  single shared job queue (`Scheduler::take_receiver`) is gone:
  `WorkerService.Stream` now reads the worker id from the worker's
  `Hello`, registers a per-worker job channel in the shared
  `ConnectedWorkers`, pumps that worker's jobs, and deregisters on
  disconnect. `Scheduler::execute` routes each action to a specific
  worker — candidates = connected workers, narrowed to the
  platform-matching healthy ones when a registry is wired in, then
  `Strategy::choose` (`SimpleFifo`) picks one; per-worker in-flight
  counts are incremented on dispatch and decremented when the result or
  timeout resolves. No eligible connected worker → `NoEligibleWorker`.
  `Scheduler` gained `connected_workers()`; the binary shares one
  `ConnectedWorkers` between the scheduler and the worker service (the
  scheduler owns it). Five scheduler tests updated/added (reject when
  none connected, reject on label mismatch, route-then-timeout, two-worker
  spread) plus the existing real-gRPC end-to-end. In-flight job
  reassignment when a worker disconnects is deferred to task 4 (leases).
- `brokkr-control::scheduling::BinPacking` — a second selection strategy
  (plan §16 task 3). Packs a worker toward a soft per-worker in-flight
  `cap` (prefers the most-loaded candidate under cap) before spreading to
  a fresh worker, so idle workers can scale down; falls back to
  least-loaded when every candidate is saturated. Deterministic id
  tie-break. `Scheduler::with_strategy` makes the selection strategy
  injectable, so the binary can pick `SimpleFifo` / `BinPacking` at
  startup. Six `BinPacking` unit tests + a scheduler test proving the
  injected strategy is honoured (two concurrent jobs pack onto one worker
  under `BinPacking(2)`). `LocalityAware` is deferred — it needs the
  action's input-root passed into `Strategy::choose` (a trait-signature
  change) plus per-worker locality state, so it gets its own increment.
- ADR 0009 — job leases, a global pending queue, and crash reassignment
  (the §16 DoD "worker crash mid-job → job retried on another worker").
  `docs/architecture/0009-leases-and-fair-scheduling.md`.
- `brokkr-control::lease::LeaseTable<P>` (plan §16 task 4 foundation):
  tracks active job leases (`JobId → {worker, deadline, payload}`),
  generic over the re-dispatch payload and clock-injected. `complete`
  resolves a lease on report (returns the payload, or `None` for a late
  report to discard); `take_expired(now)` and `take_worker(id)` remove
  and return the `(job_id, payload)` pairs to requeue on lease expiry /
  worker disconnect. Pure bookkeeping; the dispatcher wiring that drives
  it is the next increment. Seven unit tests.
- `brokkr-control::Scheduler` rewritten around a global pending queue +
  job leases + an event-driven dispatcher (ADR 0009), delivering the §16
  DoD "worker crash mid-job → job retried on another worker". `execute`
  now enqueues (the result waiter survives retries) and awaits under the
  overall timeout; `try_dispatch` leases each queued job to an idle,
  eligible, connected worker (capacity 1) chosen by the `Strategy`;
  `report` completes the lease, wakes the waiter, and re-dispatches.
  **Worker disconnect requeues the worker's in-flight job for reassignment
  to another worker** (the crash-recovery path), bounded by
  `MAX_ATTEMPTS = 5`; a late/duplicate report for a job with no active
  lease is discarded (at-least-once, safe under determinism). Connect /
  disconnect are now `Scheduler::connect_worker` / `disconnect_worker`
  (the single `take_receiver` queue and `connected_workers()` accessor are
  gone); `WorkerService.Stream` calls them. Dispatch state (connected
  workers + pending queue + leases) lives under one mutex, so there is no
  inter-lock ordering to get wrong. New scheduler test:
  `disconnect_reassigns_in_flight_job_to_another_worker` (the DoD).
  NOTE: under capacity-1, `BinPacking` spreads exactly like `SimpleFifo`
  (a worker can't hold a second concurrent job); a per-worker-capacity
  knob to re-activate packing is a follow-up. Lease-*expiry* reassignment
  (slow worker, vs. disconnect) is also a follow-up; crash recovery via
  disconnect is live.
- `brokkr-control` lease-expiry reaper: `Scheduler::reap_expired_leases`
  (and the test seam `reap_expired_at(now)`) requeues jobs whose lease has
  expired — a worker that is still connected but went silent — and
  re-dispatches them, bounded by `MAX_ATTEMPTS`. `spawn_lease_reaper`
  drives it on an interval (wired into the `brokkr-control` binary at half
  the lease window); a zero interval disables it. Jobs now carry a
  per-attempt `lease_duration` capped at `min(action timeout,
  DEFAULT_LEASE_DURATION = 60s)`, so a hung worker is retried before the
  caller's deadline. The shared requeue logic (disconnect + expiry) is
  factored into `Inner::requeue_taken`. One scheduler test
  (`lease_expiry_requeues_and_redispatches_job`). NOTE: an
  expired-but-connected worker is not yet excluded from the re-dispatch
  (it may be re-picked); "reassign strictly elsewhere" needs lease renewal
  / tried-worker tracking — a follow-up.
- `brokkr-control` lease renewal via heartbeat: each worker heartbeat now
  renews the leases that worker holds (`Scheduler::renew_worker_leases` →
  `LeaseTable::renew_worker`), so a lease expires only when a worker stops
  heartbeating (dead / partitioned) rather than merely running a long
  action. This aligns lease lifetime with heartbeat liveness and resolves
  the I13 caveat: a live, heartbeating worker never has its lease expire,
  so it can't be re-picked; only a genuinely silent worker's job is
  reassigned (and that worker is also evicted from the registry). No proto
  or worker-side change — it reuses the existing `Heartbeat` RPC. Two
  `LeaseTable::renew_worker` unit tests.
- ADR 0010 — tenants + weighted fair scheduling: tenant id from a gRPC
  metadata header (`x-brokkr-tenant`, default fallback) and virtual-time
  Start-time Fair Queuing over per-tenant-tagged pending jobs.
  `docs/architecture/0010-tenants-and-fair-scheduling.md`.
- `brokkr_common::TenantId` — tenant identifier newtype (`Default` =
  `"default"`, `DEFAULT_TENANT`), same validation as the other id
  newtypes. Two unit tests.
- `brokkr-control::fairqueue::FairQueue<J>` (plan §16 task 4 foundation):
  a pure, generic Start-time Fair Queue. `push(tenant, job)` assigns an
  SFQ virtual start tag (`start = max(virtual_time, last_finish[tenant])`,
  tenant clock += `cost/weight`, unit cost); `slots()` + `take(index)` let
  the scheduler dequeue the lowest-start-tag *dispatchable* job (respecting
  worker eligibility); `pop()` / `set_weight` / `retain` round it out.
  Seven unit tests (single-tenant FIFO, equal-tenant interleave,
  weight-proportional service, eligibility-filtered take, idle-tenant
  no-hoard). Wiring into the scheduler is the next increment.
- `brokkr-control` fair scheduling wired end-to-end (ADR 0010, the §16
  "two tenants get fair share" DoD): the scheduler's pending queue is now
  the per-tenant `FairQueue`. The `Execution` service reads the tenant from
  the `x-brokkr-tenant` request metadata header (default `"default"`) and
  threads a `TenantId` into `Scheduler::execute` and each `PendingJob`;
  `try_dispatch` dequeues the lowest-virtual-start-tag job that has an idle
  eligible worker; requeue (disconnect / lease expiry) and timeout cleanup
  re-tag / retain through the fair queue. New scheduler test
  `two_tenants_share_a_worker_fairly` — two tenants' jobs interleave on one
  worker rather than draining one-tenant-first. Per-tenant quotas
  (max-concurrent) are the next increment.
- `brokkr-control` per-tenant max-concurrent-jobs quota (ADR 0010,
  completes §16 task 4). `Scheduler::with_tenant_quota` sets an optional
  per-tenant limit (`None` = unlimited); `execute` counts a tenant's
  in-flight jobs (queued + leased) and rejects admission over the limit
  with the new typed `ExecutionError::QuotaExceeded(limit)` → gRPC
  `RESOURCE_EXHAUSTED` (code 8), before the job is enqueued. The count is
  incremented under the same lock as the enqueue (so concurrent submits
  can't both slip past) and decremented when the `execute` call goes
  terminal (success / timeout / failure), so completing a job frees the
  slot. Two scheduler tests (over-quota rejects; completion frees the
  slot). CPU-seconds/day and storage quotas remain follow-ups.
- ADR 0011 — authentication: OIDC/JWT bearer for clients (tenant from a
  claim, authoritative over the `x-brokkr-tenant` header), worker↔control
  mTLS, open-mode-with-warning when unconfigured.
  `docs/architecture/0011-auth.md`.
- `brokkr-control::auth` (plan §16 task 8): JWT client-auth core.
  `JwtAuth` validates a bearer token's signature (HS256 / RS256), `exp`,
  and optional `iss`/`aud`, and reads the tenant from a configured claim;
  `Authenticator` is either `Disabled` (open mode → tenant from header) or
  `Jwt` (token's tenant claim is authoritative). Eight unit tests. The
  server interceptor wiring is the next increment.
- New dependency `jsonwebtoken` (9; MIT) for JWT validation — crypto via
  `ring`, already in-tree through rustls/tonic TLS; no network I/O. Its
  transitive `time`/`simple_asn1` were pinned down (`simple_asn1` 0.6.2,
  `time` 0.3.36) to stay within MSRV 1.85 (newer `time` requires 1.88);
  lockfile-only, scoped to this dependency.
- `brokkr-control` auth wired into the server (ADR 0011, completes the §16
  task 8 client-auth path). `auth_interceptor` is a tonic interceptor that
  validates the `authorization: Bearer` JWT and injects the authoritative
  `TenantId` into request extensions; it guards the client-facing services
  (CAS, ActionCache, Capabilities, Execution) while the internal
  `WorkerService` stays mTLS-authenticated. A missing/invalid token →
  `UNAUTHENTICATED` (code 16). The `Execution` handler now prefers the
  injected tenant over the `x-brokkr-tenant` header (closing the ADR-0010
  spoofing gap). `brokkr-control` gains `--auth-jwt-hmac-secret-file` /
  `--auth-jwt-rsa-pem-file` / `--auth-jwt-issuer` / `--auth-jwt-audience` /
  `--auth-jwt-tenant-claim` flags (via `Args::authenticator`); with no key
  configured it runs open-mode with a loud `CLIENT AUTH DISABLED` warning
  (mirrors the TLS posture). Worker↔control mTLS is enforced by tonic when
  `--tls-client-ca` is set (client certs required). Three interceptor unit
  tests + a 3-case gRPC integration test (`tests/auth.rs`: no token /
  invalid token rejected, valid token accepted). Live OIDC/JWKS-URL
  discovery remains a follow-up.
- `docs/journal/phase-4.md` — Phase 4 wrap-up + exit-criteria review
  (`docs/plan.md` §11): shipped capabilities by task, the §16 DoD status
  (fair-share ✅, crash-recovery ✅, Bazel-compat ❌ tracked gap), the
  exit-criteria assessment, the deferred backlog, and a retrospective.
  Phase 4 is complete except the Bazel-compatibility DoD (tracked gap —
  needs a real `bazel` client + a runnable multi-process cluster).
- `docs/operations/running-a-cluster.md` + `scripts/run-cluster.sh` — how
  to run a real two-process Brokkr cluster (`brokkr-control` +
  `brokkr-worker`) and submit a job with `brokk run`, plus TLS/auth notes.
- `brokkr-control/tests/two_process_cluster.rs` — a two-process integration
  test that spawns the real `brokkr-control` and `brokkr-worker` binaries as
  separate OS processes and runs a job end-to-end through the `brokk` CLI,
  proving cross-process registration/dispatch (the in-process fixtures only
  exercise the gRPC path within one process). `#[ignore]` by default (needs
  the binaries built; spawns processes). Addresses the "runnable multi-node
  binary" gap from the Phase 4 wrap-up; the Bazel-compat DoD remains a
  tracked gap.

### Changed
- `README.md` refreshed to reflect reality: Phases 0–4 complete (the stale
  copy claimed only Phase 1 done with 2/3 in flight). Updates the status
  blurb, the "what works today" walkthrough (multi-worker dispatch, leases,
  fair scheduling, quotas, auth + a link to the cluster bring-up guide), the
  per-crate responsibility table (sandbox now lists seccomp + capability
  dropping + `pivot_root`; control lists the scheduler/leases/fair-queue/auth),
  and the roadmap table (Phase 5 = next), with the Bazel-compat DoD and S3
  cold tier / FUSE called out as tracked gaps.

## Phase 5 (in progress)

### Added
- `docs/raft-notes.md` — from-the-paper implementation notes for the custom
  Raft engine (`docs/plan.md` §17 task 1): server states & terms, persistent
  vs. volatile state, leader election with randomized timeouts, the RequestVote
  and AppendEntries RPCs, the election restriction, log repair, snapshots +
  InstallSnapshot, joint-consensus membership, client linearizability, and the
  five safety properties. Calls out the **Figure-8 current-term commit rule**
  (§5.4.2) as the load-bearing safety subtlety and pins a Figure-8 regression
  test for milestone I4. Maps each concept to its Brokkr milestone (I2–I9) and
  to the Phase 5 definition-of-done.
- ADR 0013 — custom Raft consensus for the metadata store. Records the four
  design decisions (owner-approved): redb schema (two tables, protobuf-encoded
  log entries), transport (`brokkr/v1/raft.proto` + an async `Transport` trait),
  randomness (a hand-rolled seeded PRNG, no new dependency), and concurrency (a
  single-task actor / event loop). Partially supersedes ADR 0003 (redb stays as
  the local engine; only the single-node assumption is replaced).
- `brokkr-raft` crate scaffold (milestone I1, `docs/plan.md` §17):
  - `Term`, `LogIndex`, and `NodeId` newtypes plus `LogEntry`, with lossless
    conversions to/from the wire protobuf (`brokkr.v1.LogEntry`).
  - `RaftError` typed error enum.
  - `Rng` — a seeded SplitMix64 PRNG for reproducible election-timeout jitter.
  - `RaftLog` — the redb-backed two-table log + hard-state store (`append`,
    `get`, `last_index`/`last_term`, `truncate_from`, `current_term`/
    `voted_for`); every mutator commits before returning.
  - `Transport`/`RaftRpc` traits with a production `TonicTransport`, a
    deterministic in-process `InMemoryTransport`, and a `RaftServiceAdapter`
    bridging a handler to the generated tonic server.
  - `brokkr/v1/raft.proto` — `RaftService` (RequestVote, AppendEntries,
    InstallSnapshot) with a conflict fast-backtrack hint on the AppendEntries
    reply.
  - 31 tests (newtype/PRNG/storage/wire round-trips + an integration test
    exercising the wire types over `turmoil`'s deterministic simulated network).
    The `turmoil` dev-dependency (plan §7/§21) is added here.
- `brokkr-raft` leader election (milestone I3, `docs/plan.md` §17): the
  `RaftNode` consensus state machine with `RequestVote` (`docs/raft-notes.md`
  §4.2), the `(lastLogTerm, lastLogIndex)` **election restriction** comparator
  (§6), randomized 150–300 ms election timeouts driven by an **injected clock +
  the seeded PRNG** (§4.1), the universal "higher term ⇒ step down + clear vote"
  rule persisted before responding (§2.2), vote counting to a majority, and
  heartbeat-driven election suppression. `RaftNode` is a synchronous single-owner
  state machine (no locks, ADR 0013 D4): callers drive it with `tick` and
  `handle_*` and it returns the messages to send, which makes elections
  deterministically testable without an async runtime. Eight election tests
  (single-node self-elect; 3-node happy election; the up-to-date comparator's
  five cases; grant-then-deny a second candidate; stale-term rejection; a leader
  stepping down on a higher term; heartbeats suppressing elections; and a **2–2
  split vote in term 1 resolving to a single leader in term 2**) run
  deterministically over an in-process cluster harness with fixed seeds.
- `brokkr-raft` log replication (milestone I4, `docs/plan.md` §17): the leader
  replicates its log to followers and advances the commit index safely.
  - The `AppendEntries` receiver now runs the full five-step consistency check
    (`docs/raft-notes.md` §5.1): reject a stale term; reject if the log has no
    entry at `prev_log_index` matching `prev_log_term` (returning a conflict
    fast-backtrack hint, §5.3); truncate only on a genuine conflict; append new
    entries; and advance the commit index to `min(leaderCommit, last new index)`.
  - The leader tracks per-peer `nextIndex`/`matchIndex`, replicates entries from
    `nextIndex`, advances both on success, and backs off on failure using the
    follower's conflict hint until the logs converge. A new `match_index` field
    on `AppendEntriesReply` lets the leader learn a follower's replicated prefix.
  - The leader commit rule advances `commitIndex` to the largest `N` with a
    majority of `matchIndex ≥ N` **and `log[N].term == currentTerm`** — the
    **Figure-8** current-term rule (§7) that prevents committing a prior-term
    entry by replica count. A client `propose` appends to the leader's log and
    replicates; a single-node cluster commits immediately.
  - Nine replication tests, including a faithful **Figure-8 regression test**: a
    prior-term entry re-replicated onto a majority by a later leader is *not*
    committed by replica count, and commits only once a current-term entry
    commits over it. Also covered: leader→follower replication + commit, conflict
    truncation (no duplication), a matching suffix not being truncated by a stale
    request, the too-short-log rejection + hint, a lagging follower catching up
    via back-off, single-node immediate commit, and `propose` on a follower
    returning `NotLeader`.
  - The recommended start-of-term **no-op** entry is deferred to the
    linearizable-read work (I8); it is a read-safety / commit-latency
    optimization, not a replication-safety requirement.
- `brokkr-raft` deterministic fault-injection simulation (milestone I5a,
  `docs/plan.md` §17 task 3): `tests/simulation.rs` drives a cluster of
  synchronous `RaftNode`s through a seeded network scheduler — message
  latency/reorder/loss, network partitions, and process crashes with restart
  from the persisted `RaftLog` — all reproducible from a fixed seed. A
  linearizability oracle asserts **State Machine Safety** (`docs/raft-notes.md`
  §8): every live node's committed log is a prefix of every other's, checked
  after every step. Five scenarios: replication under latency; a minority
  partition that cannot commit, healing consistently; leader crash → re-election
  with no committed entry lost; a crashed follower restarting and catching up
  from disk; and a 60-round soak interleaving writes with random
  partitions/crashes/restarts that never diverges. (The `turmoil`-based async
  driver with the real tonic transport is the companion milestone I5b.)
- `brokkr-raft` async event-loop driver (milestone I5b, `docs/plan.md` §17): the
  `RaftDriver` runs the synchronous `RaftNode` as a single `tokio` task — a
  `tokio::select!` loop over a periodic tick, inbound RPCs (via a channel),
  replies to its own outbound RPCs, and client proposals — dispatching each
  `Outbound` on a detached task through an injected `Transport`. The node is
  touched only inside the loop, so there is one writer and **no locks** (ADR 0013
  D4). A cloneable `RaftHandle` is both the inbound-RPC sink (it implements
  `RaftRpc`, so a server forwards peer RPCs to the node) and the client interface
  (`propose`, `status`). The driver reads "now" from `tokio::time`, so it runs on
  real time in production and on **simulated** time under `tokio::time::pause()`
  or `turmoil`. Two async tests on the paused clock exercise a real multi-node
  cluster deterministically: a 3-node cluster elects a leader and commits a
  proposal; a 5-node cluster's minority-partitioned leader cannot advance its
  commit index and the cluster re-forms one leader on heal. (The real
  **tonic-over-turmoil** transport + `turmoil` cluster test is a follow-up; it
  needs new dev-dependencies — `hyper-util` etc. — which are pending owner
  approval.)
- `brokkr-raft` tonic-over-turmoil cluster tests (milestone I5c, `docs/plan.md`
  §17 task 3): `tests/turmoil_cluster.rs` runs a real 3-node cluster —
  `RaftDriver`s over the production `TonicTransport` — with every Raft RPC
  crossing `turmoil`'s deterministic simulated network as genuine gRPC/HTTP2.
  Each host serves `brokkr.v1.RaftService` (its `RaftHandle` wrapped in
  `RaftServiceAdapter`, fed to tonic's `serve_with_incoming` from a
  `turmoil::net::TcpListener`); peer channels dial through a
  `tower::service_fn` connector opening `turmoil::net::TcpStream`s wrapped in
  `hyper_util::rt::TokioIo`. Two scenarios: election + replicated commit, and
  a partitioned leader that cannot commit while the majority elects a
  higher-term leader, then steps down and converges on heal. Peer channels
  enable **HTTP/2 keepalive** — turmoil partitions drop packets silently, so
  without it a dead h2 connection never errors and tonic would pin it forever,
  preventing re-integration after a heal (exactly the failure the first run
  caught). `RaftServiceAdapter` is now re-exported from the crate root. New
  dev-dependencies (owner-approved): `hyper-util`, `tower`, `tokio-stream` for
  `brokkr-raft`; `hyper-util` + `tower` added to the workspace manifest.
- `brokkr-raft` snapshots & log compaction (milestone I6, `docs/plan.md` §17
  task 4, Raft §7): a `SnapshotMeta { last_included_index, last_included_term }`
  plus an opaque state-machine blob persist **atomically with the covered log
  prefix's removal** in one redb transaction (`RaftLog::compact_to` /
  `install_snapshot_replacing_log`); last-`(index, term)` lookups fall back to
  the snapshot metadata so elections stay correct after the whole log
  compacts. `RaftNode::compact(data)` snapshots the committed prefix (blob
  supplied by the shell — I8's KV wires it to the state machine),
  `needs_snapshot()` reports growth past `Config::snapshot_threshold`
  (default 8192), and recovery floors the commit index at the snapshot (P9).
  A leader whose follower's `nextIndex` was compacted away ships the blob via
  a single-shot `InstallSnapshot` (chunking noted as future work); the
  receiver retains its log tail when the snapshot's last-included entry
  matches, discards the whole log otherwise, and ignores stale snapshots.
  The driver routes `InstallSnapshot` both ways, exposes
  `RaftHandle::compact`, and reports the snapshot in `DriverStatus`; a
  protocol-invalid inbound snapshot (chunked) is rejected without killing the
  event loop. The I5a simulation campaign runs green with
  `snapshot_threshold = 16` — snapshots exercised constantly under the fault
  soak — plus new scenarios: a crashed follower catching up **via
  InstallSnapshot** after the cluster compacted past its log, and
  restart-from-snapshot history equality via the linearizability oracle
  (snapshot blob + log tail).
- `brokkr-raft` entry payloads + cluster configurations (milestone I7a,
  `docs/plan.md` §17 task 5 groundwork): `LogEntry` now carries an
  `EntryPayload` — `Command(Bytes)` (the default), `Noop` (reserved for the
  linearizable-read path), or `Config(ClusterConfig)`. `ClusterConfig` models
  joint consensus per Raft §6 / thesis ch. 4: `voters`, optional `old_voters`
  (present ⇔ the joint C_old,new is in flight), and non-voting `learners`;
  `has_quorum` demands a strict majority of *both* voter sets when joint and
  never counts learners. The wire/disk protobuf grows an `EntryKind`
  discriminator and a `ClusterConfig` message with COMMAND as the proto3
  default, so **entries written before I7 keep decoding unchanged** (proven
  by a test against the exact pre-I7 encoding). Decoding is now fallible
  (`TryFrom`): unknown kinds, config-less CONFIG entries, and invalid node
  ids are rejected instead of silently mangled. No behavioral change to
  consensus yet — the dual-majority election/commit machinery,
  config-on-append semantics, and learner catch-up land as I7b/I7c.
- `brokkr-raft` joint-consensus membership changes (milestone I7b,
  `docs/plan.md` §17 task 5, Raft §6 / thesis ch. 4): the node now derives
  its membership from the log. A `ConfigTracker` holds the base config
  (snapshot/bootstrap) plus every config entry still in the log; the active
  config **applies on append** and **rolls back on truncation** (P10, tested
  head-on). The fixed `peers` list is gone: replication targets, vote
  targets, election tallies, and the commit rule all derive from the active
  config via `ClusterConfig::has_quorum` — majorities in *both* voter sets
  while C_old,new is in flight, learners never counted.
  `RaftNode::propose_conf_change(new_voters)` appends C_old,new (one change
  in flight, validated); when it commits the leader automatically appends
  C_new; when a committed C_new excludes the leader it steps down — and the
  remaining members elect among themselves. `SnapshotMeta` and
  `InstallSnapshot` now carry the configuration in effect at the snapshot
  point (proto field 8 + persisted `snapshot_config`), so a node restoring
  from snapshot learns its membership from it, overriding the bootstrap
  list. **Leader stickiness (thesis §4.2.3)** ships with this: a server
  that heard from a current leader within the minimum election timeout
  disregards RequestVote (no term bump, no grant). The driver-level removal
  test caught the omission — without it, the *removed* server's endless
  campaigns deposed the leader of the cluster it had left; a stale minority
  leader still steps down via AppendEntries terms, so liveness holds.
  Driver: `RaftHandle::propose_conf_change` + `DriverStatus.config`.
- `brokkr-raft` learners + catch-up gate (milestone I7c, `docs/plan.md` §17
  task 5, thesis ch. 4 — completes I7): fresh nodes join as **non-voting
  learners** and are promoted only once caught up. `RaftNode::new_learner`
  bootstraps a joining node whose config lists itself only as a learner;
  **only voters campaign** — a learner (or a node its active config no
  longer names) re-arms its election timer instead of bumping terms it can
  never win with. `propose_add_learner` appends a single non-joint config
  entry (a learner changes no quorum), still under the one-in-flight rule.
  `propose_conf_change` now enforces the **catch-up gate**: every *added*
  voter must be replicated to within `Config::catch_up_margin` (default 256)
  entries of the leader's log, else the change is refused with a pointer to
  the learner flow — without the gate, promoting a fresh node adds a voter
  that cannot ack anything and can stall commitment. Driver:
  `RaftHandle::propose_add_learner`. **The I7 exit criterion is met**: the
  fault campaign runs green with membership churn in the mix
  (`soak_random_faults_with_membership_churn` — spares join as learners,
  promote through joint consensus, founders retire, all under random
  partitions/crashes/restarts with compaction at threshold 16, the
  linearizability oracle checked every round).
- `brokkr-raft` persistent state (milestone I2, `docs/plan.md` §17): the Raft
  hard state (`currentTerm` + `votedFor`) is now modelled by `HardState` and
  written **atomically as a unit** via `RaftLog::save_hard_state` in a single
  durable redb transaction, replacing the separate `set_current_term` /
  `set_voted_for` writes. This closes a **torn-vote** hazard: a crash between
  bumping the term and clearing the vote could otherwise leave a new term paired
  with a stale vote and let a node vote twice in one term (violating Election
  Safety). Crash-consistency is proven by tests: an uncommitted write is
  invisible after reopen, term-step clears the vote atomically, and a committed
  hard state survives a real process abort (`tests/crash_consistency.rs`
  re-execs the test binary as a child that commits then `abort()`s).
