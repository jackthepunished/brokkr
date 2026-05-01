# Phase 2 — Hermetic Sandboxing

- **Status:** in progress
- **Plan:** `docs/phase-2-plan.md`
- **Started:** 2026-04-30

This journal accumulates short retrospectives as each milestone (M1–M9)
lands. Each milestone is a single PR; each section here is the post-merge
debrief.

## M1 — `feat/phase2-host-check`

- **Date:** 2026-04-30
- **PR:** _(filled in after merge)_
- **Outcome:** `brokkr-worker --check-host` runs seven probes against the
  host kernel and prints a per-probe `[ OK | WARN | FAIL ]` checklist. A
  one-shot `scripts/install-cgroup-slice.sh` creates the
  `/sys/fs/cgroup/brokkr.slice` cgroup, chowns it, and enables the
  controllers we'll need (cpu/memory/pids/io). The journal you're reading
  was started in this milestone too.

### Open questions resolved

- **Cgroup delegation default** (plan §11.1). Going with the manual chown
  path — `scripts/install-cgroup-slice.sh` — as the documented default.
  systemd's `Delegate=yes` keeps working; documenting both paths is M9.
- **`unprivileged_userns_clone` is Debian/Ubuntu-only.** Probe via the
  universal `/proc/sys/user/max_user_namespaces` first; treat the
  Debian/Ubuntu sysctl as an *additional* gate, not the only one. Saves us
  spurious failures on Fedora / RHEL / Arch.

### Open questions deferred

- **`brokkr-sandboxd` discovery** (plan §11.2). Not relevant until M2
  when the binary actually exists. Decision: relative path next to the
  worker binary, with `$PATH` fallback.
- **systemd vs no-systemd hosts** (plan §11.5). M1 documents only the
  no-systemd path; M9 will add a systemd unit file.

### What surprised me

- `/proc/sys/kernel/unprivileged_userns_clone` is *not* universal —
  Debian/Ubuntu added it as an extra hardening knob. The cross-distro probe
  has to look at `/proc/sys/user/max_user_namespaces` (universal) and
  *also* honour the Debian/Ubuntu sysctl when it's present.
- Cgroup write-tests can't use `open(O_RDWR)` reliably — the only
  trustworthy probe is `mkdir`-then-`rmdir`. The directory name needs the
  PID baked in to avoid collisions if two workers run `--check-host`
  concurrently.
- Kernel release strings on WSL2 look like `6.6.87.2-microsoft-standard-WSL2`
  — the parser splits on both `.` and `-` to extract the leading
  `(major, minor)` pair without choking on the suffix.

## M2 — `feat/phase2-sandbox-skeleton`

- **Date:** 2026-04-30
- **PR:** #24
- **Outcome:** Full public type surface (`Sandbox`, `SandboxConfig`,
  `SandboxOutcome`, `SandboxError`, `RootfsSpec`, `ResourceLimits`,
  `NetworkPolicy`, `DeterminismPolicy`, `StdioPolicy`,
  `ResourceAccounting`, `SandboxTimings`, `ExitStatus`) lands on day
  one. `brokkr-sandboxd` re-exec runner reads JSON config from fd 3 and
  `execvpe`s the action — Phase-1 parity inside the new process model,
  no namespace work yet.

### Open questions resolved

- **Crate split** (plan §3.2): co-located `brokkr-sandboxd` as a
  `[[bin]]` target inside `brokkr-sandbox` rather than a separate
  crate. Reason: `CARGO_BIN_EXE_brokkr-sandboxd` is automatically set
  for tests in the same package, so integration tests find the runner
  without `escargot` or build-script gymnastics. Doesn't affect the
  runtime model (still a separate process via re-exec).
- **`brokkr-sandboxd` discovery** (plan §11.2):
  `Sandbox::with_default_runner()` checks next to the worker exe, then
  `$PATH`. Returns `SandboxError::Unsupported` if neither has it.

### What surprised me

- `pipe2(O_CLOEXEC)` is load-bearing. Without it, the runner inherits
  a copy of its own write end through `fork`, `read_to_end(fd 3)`
  never sees EOF, and the host deadlocks on `wait_with_output`. With
  it, the inherited write end auto-closes on `execve` and the
  invariant holds. Took ~14 minutes of staring at hung sandboxd
  processes to localise.
- `dup2(N, N)` is a no-op that does **not** clear `FD_CLOEXEC` on the
  target. So when `pipe2` happened to return the read end already at
  fd 3, our `if child_read_fd != 3 { dup2 }` skip-branch left the
  CLOEXEC inherited from `O_CLOEXEC`, and the runner's fd 3 closed on
  exec. Fix: explicit `fcntl(F_SETFD, empty)` on that branch. (Found
  during Copilot review of #24.)
- CI flake on the workdir test traced to write-side `EPIPE` when the
  runner exited before the host finished writing. Made the host
  tolerate `EPIPE` and surface the runner's stderr as
  `RunnerCrashed` — the runner's diagnostic is always more useful
  than "Broken pipe".

## M3 — `feat/phase2-mount-namespace`

- **Date:** 2026-04-30
- **PR:** _(filled in after merge)_
- **Outcome:** Runner enters its own user namespace + mount namespace,
  builds a tmpfs rootfs from `RootfsSpec.{ro_binds, tmpfs, symlinks}`,
  and `pivot_root`s into it. Host's mount tree is unreachable inside.
  Three new evil-action tests pass (EV-01 cat /etc/shadow, ls / shows
  only what we put there, EV-15 host mountinfo unchanged).

### Decisions

- **User-ns single-mapping form.** `0 <host_uid> 1` (and the
  equivalent gid map). It's all the kernel allows an unprivileged
  process to write directly without `newuidmap`/`/etc/subuid`. Wider
  ranges land in a follow-up if a real workload needs them.
- **`unshare(NEWUSER | NEWNS)` in one call.** Per `clone(2)`, when
  multiple `CLONE_NEW*` flags are set together with `CLONE_NEWUSER`,
  the user ns is created first — so the new mount ns is born with
  full caps. Saves an `unshare` round trip and makes the sequence
  obvious.
- **Skip mount-ns path when `RootfsSpec` is empty.** Default-empty
  spec → runner runs the action against the host filesystem, M2
  behaviour. Lets the M2 smoke tests stay un-modified instead of
  forcing every caller to opt out.

### What surprised me

- `setgroups` deny **must** be written before `gid_map` in an
  unprivileged user namespace. Forgetting it returns `EPERM` from the
  `gid_map` write with no other diagnostic. (Plan §5.3 called this
  out; still earned its mention in the journal.)
- Bind-mounting a path that's a symlink (`/lib`, `/lib64` on usrmerge
  hosts) can succeed-but-do-the-wrong-thing or fail outright. The
  M3 default rootfs detects symlinked roots with
  `is_dir() && !is_symlink()` and falls back to creating a symlink
  inside the tmpfs instead.
- A read-only bind takes **two** `mount(2)` calls: a regular bind,
  then a `MS_REMOUNT | MS_BIND | MS_RDONLY` to flip the read-only
  flag. The first call ignores `MS_RDONLY`; the man page documents
  this as "fs-independent flags only take effect on remount".

## M4 — `feat/phase2-pid-namespace`

- **Date:** 2026-04-30
- **PR:** _(filled in after merge)_
- **Outcome:** Runner adds `CLONE_NEWPID` to its `unshare` and forks
  twice. Outer runner waits on init; init (PID 1 in the new pidns)
  mounts `/proc`, forks the action (PID 2), and reaps orphans until
  the action exits. Both layers translate the child's `WaitStatus` to
  their own exit (`process::exit(code)` or `signal::raise(sig)` after
  restoring `SigDfl`), so the host's `ExitStatus::Exited` /
  `ExitStatus::Signaled` mapping is unchanged. AC-01, EV-13, EV-16
  pass; the existing M3 tests continue to pass with one tweak (`/proc`
  is now in the sandbox root).

### Decisions

- **Two forks, not one.** `unshare(CLONE_NEWPID)` does *not* move the
  caller into the new pidns — its next fork lands there as PID 1. So
  the outer runner stays in the host pidns (necessary so the host can
  `waitpid` it), and we need an init child to be PID 1, plus an
  action grandchild so PID 1 can do its reaper job without being the
  thing that execs the action. Three processes, two `fork(2)` calls,
  one `execvpe`.
- **Mount `/proc` inside init, not in the outer runner.** Procfs
  reflects the *reader's* PID namespace, so a `/proc` mounted before
  the pidns split would show the host's PIDs. Init mounts it
  post-fork (and post-pivot — `setup_rootfs` `mkdir /proc` so the
  mount point exists when init gets there).
- **Signal re-raise instead of `_exit(128 + sig)`.** When the action
  is killed by a signal, init / outer runner restore the default
  disposition for that signal and `raise()` it on themselves so the
  host sees `ExitStatus::Signaled { signal: <orig> }`. Falling back
  to `process::exit(128 + sig)` only on the (unreachable) path where
  re-raise didn't actually kill us. This keeps Phase 2's exit-status
  contract identical to Phase 1's plain-process model.

### What surprised me

- `impl FnOnce() -> !` is gated behind the unstable `never_type`
  feature even though `-> !` on a free function compiles fine on
  stable. Workaround: drop the closure-shaped API and inline the two
  forks at the call site, exposing only helpers (`exit_with`,
  `mount_proc`, `reap_until`) from `pidns.rs`. Less elegant on paper
  but kept the runner readable and stable-Rust-clean.
- `libc::_exit` is *not* declared `-> !` in the Rust binding —
  `std::process::exit` is. Switched to the std variant; we accept
  that Rust runs atexit handlers (we don't register any in the
  runner) in exchange for a function that's actually divergent in the
  type system.
- nix's `signal::raise` lives behind the `signal` cargo feature,
  separate from the existing `process` and `sched` features. Easy to
  miss because `nix::sys::signal::Signal` itself is in scope; the
  failure shows up as a "missing function" error rather than a
  "missing module" error.

## M5 — `feat/phase2-network-namespace`

- **Date:** 2026-04-30
- **PR:** _(filled in after merge)_
- **Outcome:** `unshare` now also asks for `CLONE_NEWNET`, so the
  action gets an empty network namespace by default — no interfaces,
  no routes, not even loopback. `NetworkPolicy::Loopback` brings `lo`
  up via a hand-rolled `RTM_NEWLINK` netlink message. Three EV/AC
  tests (`net_ns.rs`) prove this with errno-level assertions: 1.1.1.1
  is `ENETUNREACH`, 127.0.0.1 is `ENETUNREACH` with policy=None and
  `ECONNREFUSED` with policy=Loopback.

### Decisions

- **Hand-roll the netlink, don't pull in `rtnetlink`.** The
  `rtnetlink` crate works but drags in a tokio-flavoured async stack
  (futures, channels, an executor) for what's a single 32-byte
  request and a single ack. Two `repr(C)` structs and ~150 lines of
  raw libc were cheaper than the dep tree review.
- **Use `nix::libc` instead of taking a direct `libc` dep or the
  `nix` `socket`/`net` features.** nix already re-exports `libc`, and
  AF_NETLINK / `if_nametoindex` are in `libc` proper. Adding the nix
  feature would have been one line in `Cargo.toml` but pulled
  hundreds of lines of nix wrappers we don't want.
- **Apply the network policy in the outer runner, before fork.** The
  netns is created at `unshare`, so it's already correct when init
  starts; doing the loopback bring-up before fork keeps init's job
  small (still just mount `/proc` + reap) and lets us surface
  netlink failures with the same `die("apply network policy", …)`
  diagnostic plumbing as the other setup steps.
- **Errno through exit code as the EV-test contract.** Using
  `python3 -c '... sys.exit(e.errno)'` is the cleanest way to
  distinguish `ENETUNREACH` (101) from `ECONNREFUSED` (111). Both fit
  in the 0–255 exit-code window. The alternative (parsing stderr from
  `bash`'s `/dev/tcp` redirect) is brittle.

### What surprised me

- `127.0.0.1` is `ENETUNREACH`, not `ECONNREFUSED`, when `lo` is
  `DOWN`. I'd expected the kernel to fall back to a generic
  "no route" or to silently drop, but it's the same `ENETUNREACH`
  that an external IP gets — useful, because it means policy=None
  vs policy=Loopback is distinguishable from inside the action via
  errno alone.
- The kernel always replies to netlink requests with
  `NLM_F_ACK` set, even on success — the ack is an `NLMSG_ERROR`
  message with `errno=0`. Surprised me on first read of the spec;
  documented in `man 7 netlink`.
- `c"lo"` (a C-string literal in edition 2024 / Rust 1.85) is a
  cleaner spelling than `b"lo\0"`. clippy actually fired on the
  byte-string version (`manual_c_str_literals`).

## M6 — `feat/phase2-cgroups`

- **Date:** 2026-04-30
- **PR:** _(filled in after merge)_
- **Outcome:** Per-action cgroup-v2 created under a configurable
  slice root; the runner pid is attached before the runner makes
  any progress, so the action and all its descendants live inside
  bounded `memory.max` / `pids.max` / `cpu.max`. Wall-clock timeout
  fires via `tokio::time::timeout` and uses `cgroup.kill` to
  atomically tear down the whole tree. OOM kills are detected via
  `memory.events:oom_kill > 0` and surfaced as
  `ExitStatus::OutOfMemory`. Accounting (`cpu.stat`, `memory.peak`,
  `pids.peak`, `io.stat` aggregated) is read after the action exits.
  Four new tests; the wall-clock one runs everywhere, the cgroup
  ones skip unless we have a writable delegated slice.

### Decisions

- **Builder, not constructor.** `Sandbox::with_cgroup_root(path)` is
  optional. Without it, the sandbox is M2-M5 — no cgroup, accounting
  stays at zero. This kept all 16 existing tests passing without
  touching them, and lets workers that don't have a delegated slice
  still run actions (with no resource limits).
- **Attach happens between spawn and config write.** The runner is
  parked on `read_to_end(fd 3)` immediately after exec because the
  pipe stays open until the host closes its writer end. We exploit
  that gap: spawn → attach pid → write config → close. By the time
  the runner unblocks and forks into init/action, every PID in the
  tree inherits the cgroup automatically. No barrier protocol needed.
- **`cgroup.kill` over per-pid loops.** Kernel ≥ 5.14 has a single
  atomic write that SIGKILLs every PID in the cgroup at once. We try
  it first; on ENOENT we fall back to walking `cgroup.procs` and
  `kill(pid, SIGKILL)`-ing each. Important for wall-clock timeout
  on actions with grandchildren (e.g., `sh -c '... &'`).
- **Stop using `wait_with_output`.** It consumes the `Child`, so
  there's no handle to `kill()` on timeout. Now we `take()` stdout
  and stderr, spawn drain tasks, and `wait()` the child directly.
  Same observable behaviour, but interruptible.
- **OOM > Signaled override.** When the OOM-killer fires, the kernel
  reports the action's `WaitStatus` as `Signaled(SIGKILL)`. That's
  technically true but useless to the caller — they want to know
  it was OOM, not "killed by something". So after `wait`, we check
  `memory.events:oom_kill` and overwrite the exit status if the
  kernel says yes. Same idea for `Timeout`.
- **Test skip via "can move ourselves into it" probe.** Tests use a
  two-step probe: mkdir a unique leaf, then write our own pid into
  its `cgroup.procs`. The second step catches the cgroup-v2
  cross-delegation rule that mkdir alone can't see — a slice we can
  mkdir under may still reject our `cgroup.procs` write if our
  source cgroup is in a different delegation tree. CI / fresh WSL2
  fail at the second step and skip cleanly.

### What surprised me

- Cgroup-v2 cross-delegation rules are stricter than I'd remembered.
  Even with the slice chowned to our user, you can't move a process
  into it from `/init.scope` (root-owned source) — the kernel checks
  write permission on the *source* cgroup too, and on the common
  ancestor. The fix in real deployments is to run the worker
  *inside* the delegated tree (systemd unit with `Delegate=yes`, or
  `systemd-run --user --scope`); for tests we just probe and skip.
- A naive bash fork bomb (`f() { f & f; }; f`) under `pids.max`
  doesn't *die* — bash retries `clone()` forever, printing
  "Resource temporarily unavailable" until the wall-clock timeout
  fires. Switched to a python loop that exits on first `OSError`
  for a deterministic non-zero exit. The test still has a wall-clock
  guard as a backstop.
- `memory.swap.max` doesn't exist on kernels without the swap
  controller; we ignore ENOENT on the write so a missing controller
  doesn't block the run.
- `tokio::process::Child::wait_with_output` consumes the child by
  value, which makes timeout-with-kill awkward. The fix (split
  stdout/stderr off, spawn drains, wait separately) is exactly
  what `wait_with_output` does internally — we just had to inline
  it to keep a `&mut Child` for `kill()`.
