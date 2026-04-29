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
