# Phase 2 — Hermetic Sandboxing: Implementation Plan

> **Scope.** This document elaborates `docs/plan.md` §14 ("Phase 2 — Hermetic
> Sandboxing") into a buildable, reviewable plan. Every design decision is
> motivated; every milestone is independently shippable; every test in the
> evil-action matrix maps to a kernel facility we are exercising.
>
> **Audience.** Anyone (human or LLM) implementing Phase 2 work.
> Read `docs/plan.md` first; this file assumes that context.
>
> **Hard constraint** (`CLAUDE.md` rule #9): no Docker, runc, containerd,
> bubblewrap, or any other container runtime. The sandbox is built on raw
> Linux primitives. That is the educational point of the project.

---

## 1. Goal & Non-Goals

### 1.1 Goal

After Phase 2, `brokkr-worker` executes every action inside a Linux sandbox
that satisfies these properties simultaneously:

1. **Filesystem isolation.** The action sees a minimal, deterministic root
   filesystem. It cannot read `/etc/shadow`, `/home/$USER`, host SSH keys,
   build secrets, or any path outside its declared input + output trees.
2. **Process isolation.** The action cannot `ptrace`, signal, or even
   observe processes outside its sandbox. Inside, it is PID 1 of a fresh
   PID namespace.
3. **Network isolation.** The action has no network connectivity by default.
   Loopback is opt-in via `Action.Platform`.
4. **Resource isolation.** CPU time, memory, pid count, and IO throughput
   are bounded by the cgroup. Exceeding the memory limit terminates the
   action with a structured `OutOfMemory` error rather than a host OOM kill.
5. **Privilege isolation.** The action runs as UID 0 *inside* its user
   namespace, mapped to a dedicated unprivileged UID *outside*. All file
   capabilities are dropped. `no_new_privs` is set.
6. **Syscall isolation.** A default-deny seccomp-bpf filter blocks
   everything except the syscalls real build tools need.
7. **Determinism.** Hostname, timezone, environment, and other ambient
   inputs are normalized so two runs of the same action produce
   bit-identical outputs.
8. **Accountability.** Every action records CPU-time used, peak RSS, and
   bytes read/written, retrievable from the `ActionResult`.

### 1.2 Non-Goals

These belong to later phases and must not creep into Phase 2:

- **Distributed CAS / FUSE input materialization** — Phase 3.
  Phase 2 still copies inputs from the local on-disk CAS into the sandbox.
- **Multi-tenant scheduling, fair-share, priority** — Phase 4.
- **High availability of the control plane / Raft** — Phase 5.
- **Network sandboxing beyond "on/off + loopback"** — egress allowlists,
  per-action netns with bridges, etc. are deferred.
- **GPU / device passthrough.**
- **macOS / non-Linux support.** The sandbox is Linux-specific. Other hosts
  will run the worker without a sandbox at first; running real actions on
  them is out of scope until at least Phase 6.

---

## 2. Threat Model & Trust Boundaries

### 2.1 Adversary

The action is **untrusted**. It may be a build that has been tampered with,
a remote-execution job from another tenant, or simply a benign tool that
becomes malicious because of an exploited dependency. We assume the
adversary controls `argv[0]` and everything it execs into.

We do **not** defend against:

- A compromised control plane (the control plane is trusted in Phase 2;
  multi-tenant scheduling and per-tenant authentication are Phase 4).
- A compromised worker host kernel.
- Side-channel attacks against co-tenants on the same host (Spectre, RAMBleed,
  cache timing). Mitigations are out of scope.
- Hardware-level attacks (DMA, evil-maid).

### 2.2 Trust boundaries

```
+--------------------+        gRPC        +--------------------+
|   brokkr-control   | <----------------> |   brokkr-worker    |
|   (trusted)        |                    |   (trusted)        |
+--------------------+                    +--------------------+
                                                    |
                                          spawn + IPC (fd-passed config)
                                                    v
                                          +--------------------+
                                          |  brokkr-sandboxd   |  ◄── trusted helper
                                          |  (re-exec runner)  |
                                          +--------------------+
                                                    |
                                          enter namespaces, drop caps,
                                          apply seccomp, exec action
                                                    v
                                          +--------------------+
                                          |   action process   |  ◄── UNTRUSTED
                                          |   (PID 1 in netns) |
                                          +--------------------+
```

Everything to the left of "spawn" is trusted code. Everything below the
namespace boundary is untrusted. The sandboxd runner straddles the boundary:
it runs trusted code, but its output (stdout, stderr, exit code, resource
counters) is what gets reported back to the worker.

### 2.3 What an escape costs us

A successful sandbox escape lets the action read host files, talk to the
network, or persist outside its workspace. A successful resource exhaustion
(unbounded memory, fork bomb) lets the action degrade the host for other
tenants. Both are showstoppers for production but not for Phase 2 — we ship
Phase 2 with the goal of *materially raising the cost of escape*, not of
formal proof.

---

## 3. Architecture

### 3.1 The re-exec process model

Setting up Linux namespaces from a multi-threaded async runtime is a
minefield. `clone(CLONE_NEWUSER)` requires the calling process to be
single-threaded; `fork()` between threads leaves the child in an undefined
state for everything that isn't async-signal-safe; `pivot_root` and
`pivot_root`'s rules about mount propagation interact with the worker's
own filesystem state.

Every serious Linux sandbox (bubblewrap, crun, runj, gVisor's `runsc`,
nsjail) solves this with the same pattern: a separate, small,
single-threaded **runner binary** that does namespace setup, applies
seccomp, drops capabilities, and `execve`s the user's command. The host
process spawns the runner via `posix_spawn` + a pre-prepared config and
reads back stdout / stderr / exit code over pipes.

We adopt that pattern. There will be:

- A library crate `brokkr-sandbox` exporting the public Rust API
  (`Sandbox`, `SandboxConfig`, `SandboxOutcome`, `SandboxError`).
- A binary crate `brokkr-sandboxd` (a tiny `main` over the library's
  `runner_main`) that the worker spawns once per action. Naming chosen
  to avoid collision with `brokkr-sandbox` (the lib) and to be obvious in
  `ps` output. The `d` is for "daemon-like helper", not "long-running".

The library exposes both halves: the spawning side (`Sandbox::run`) and
the runner-side entry point (`run_as_runner`) so a single binary can host
both modes if useful. The default deployment is a separate binary.

### 3.2 Crate layout

```
crates/brokkr-sandbox/
├── Cargo.toml
├── src/
│   ├── lib.rs            # Public types: Sandbox, SandboxConfig, SandboxOutcome, SandboxError
│   ├── config.rs         # SandboxConfig + serde (used as IPC payload)
│   ├── error.rs          # SandboxError (thiserror)
│   ├── host/             # The "spawn the runner" side, runs in the worker
│   │   ├── mod.rs        # Sandbox::run
│   │   ├── cgroup.rs     # Per-action cgroup creation, limits, accounting readback
│   │   ├── workspace.rs  # Stage input root on host, layout output dirs
│   │   └── ipc.rs        # Pipe / fd-passing protocol with the runner
│   ├── runner/           # The "inside the sandbox" side, runs after re-exec
│   │   ├── mod.rs        # runner_main entrypoint
│   │   ├── mount.rs      # Build rootfs in tmpfs + pivot_root
│   │   ├── userns.rs     # Set up uid_map / gid_map
│   │   ├── pidns.rs      # PID 1 reaper loop
│   │   ├── netns.rs      # Network namespace setup (loopback toggle)
│   │   ├── seccomp.rs    # Default-deny BPF filter, additive allowlist
│   │   ├── caps.rs       # Drop file capabilities, set NO_NEW_PRIVS
│   │   ├── determinism.rs# Hostname, TZ, env scrubbing
│   │   └── exec.rs       # Final execve into the user's command
│   └── platform/
│       └── linux.rs      # syscall + libc wrappers; non-Linux returns Unsupported
└── tests/
    ├── evil/             # The evil-action matrix (§8.1)
    └── good/             # Real-world acceptance (§8.2)

crates/brokkr-sandboxd/
├── Cargo.toml
└── src/main.rs           # ~10 lines: read config from fd 3, call runner_main
```

### 3.3 Lifecycle of a single action (Phase 2)

```
worker.handle_job()
  ├── 1. Allocate per-action cgroup       (host/cgroup.rs)
  ├── 2. Stage input tree on host          (host/workspace.rs)
  ├── 3. Build SandboxConfig               (config.rs)
  ├── 4. Sandbox::run(config)              (host/mod.rs)
  │     ├── 4a. Open pipes (config-fd, stdout, stderr)
  │     ├── 4b. posix_spawn(brokkr-sandboxd)
  │     ├── 4c. Write config JSON to config-fd, close
  │     ├── 4d. Move runner pid into the cgroup
  │     ├── 4e. Wait for runner to exit, drain pipes
  │     └── 4f. Read cgroup accounting counters
  ├── 5. Stat output files into ActionResult
  ├── 6. Upload stdout/stderr/outputs to CAS
  ├── 7. Tear down cgroup, remove workspace
  └── 8. Send JobResult upstream

inside brokkr-sandboxd (after re-exec):
  ├── A. Parse SandboxConfig from fd 3
  ├── B. unshare(CLONE_NEWUSER|NEWNS|NEWPID|NEWNET|NEWUTS|NEWIPC|NEWCGROUP)
  ├── C. Write uid_map / gid_map for the new user namespace
  ├── D. Build minimal rootfs in fresh tmpfs
  ├── E. pivot_root + umount old root
  ├── F. Mount /proc, minimal /dev, /sys (read-only)
  ├── G. Set hostname, TZ, scrub env
  ├── H. Drop all capabilities, set NO_NEW_PRIVS
  ├── I. Apply seccomp-bpf filter
  ├── J. Fork; child execve()s the action; parent loops on wait4 (PID 1 reaper)
  └── K. On action exit, write exit_code to a status pipe, then exit
```

Steps 4d (cgroup attach) is done by the worker, not the runner, because
the runner inside its new user namespace cannot write to the host cgroup
hierarchy. The worker writes the runner's PID into `cgroup.procs` *before*
the runner enters its namespaces. cgroups follow the process; the runner
brings its cgroup membership with it.

---

## 4. Public API

```rust
// brokkr-sandbox/src/lib.rs

/// A sandbox capable of executing a single action.
pub struct Sandbox { /* host-side handle */ }

/// Configuration for one action's sandbox. Serializable: this is also the
/// IPC payload sent from the host process to the runner over fd 3.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxConfig {
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub workdir: PathBuf,           // path inside the sandbox, e.g. "/work"
    pub rootfs: RootfsSpec,         // §5.1
    pub limits: ResourceLimits,     // §5.5
    pub network: NetworkPolicy,     // §5.4
    pub stdin: StdioPolicy,         // §5.8
    pub determinism: DeterminismPolicy, // §5.8
    /// Capabilities to retain (default: empty). Phase 2 default is none.
    pub retained_caps: Vec<Capability>,
    /// Additional syscalls to allow on top of the default whitelist.
    pub extra_seccomp_allow: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RootfsSpec {
    /// Read-only host paths to bind into the rootfs. Each entry is
    /// (host_path, sandbox_path).
    pub ro_binds: Vec<(PathBuf, PathBuf)>,
    /// Read-write tmpfs mounts. Each entry is (sandbox_path, size_bytes).
    pub tmpfs: Vec<(PathBuf, u64)>,
    /// Input tree to copy/bind under workdir.
    pub input_root: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ResourceLimits {
    pub cpu_milli: Option<u64>,     // cpu.max
    pub memory_bytes: Option<u64>,  // memory.max
    pub pids_max: Option<u64>,      // pids.max
    pub io_read_bytes_per_sec: Option<u64>,
    pub io_write_bytes_per_sec: Option<u64>,
    pub wall_clock: Option<Duration>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum NetworkPolicy {
    /// New empty netns. No interfaces, no routes.
    None,
    /// New netns with `lo` brought up.
    Loopback,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct DeterminismPolicy {
    pub hostname: Option<String>,            // default "brokkr-sandbox"
    pub timezone_utc: bool,                  // default true
    pub source_date_epoch: Option<i64>,      // SOURCE_DATE_EPOCH
    pub strip_ld_preload: bool,              // default true
    pub strip_path: bool,                    // replace PATH with a fixed default
}

/// Outcome of a sandboxed action.
#[derive(Debug)]
pub struct SandboxOutcome {
    pub exit_status: ExitStatus,
    pub stdout: Bytes,
    pub stderr: Bytes,
    pub accounting: ResourceAccounting,
    pub timings: SandboxTimings,
}

#[derive(Debug, Clone, Copy)]
pub enum ExitStatus {
    Exited(i32),
    Signaled { signal: i32, core_dumped: bool },
    /// Killed by the cgroup OOM. The action's writable outputs may be partial.
    OutOfMemory,
    /// Wall-clock limit hit; the runner sent SIGKILL.
    Timeout,
}

#[derive(Debug, Clone, Copy)]
pub struct ResourceAccounting {
    pub cpu_user: Duration,
    pub cpu_system: Duration,
    pub memory_peak_bytes: u64,
    pub io_read_bytes: u64,
    pub io_write_bytes: u64,
    pub max_pids: u64,
}

#[derive(thiserror::Error, Debug)]
pub enum SandboxError {
    #[error("sandbox setup failed at step {step}: {source}")]
    Setup { step: &'static str, source: std::io::Error },
    #[error("the kernel does not support a feature we require: {0}")]
    Unsupported(&'static str),
    #[error("the sandbox runner exited abnormally before exec: {0}")]
    RunnerCrashed(String),
    #[error("cgroup operation failed: {0}")]
    Cgroup(#[source] std::io::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

impl Sandbox {
    pub fn new(host_data_dir: &Path) -> Result<Self, SandboxError> { ... }
    pub async fn run(&self, cfg: SandboxConfig) -> Result<SandboxOutcome, SandboxError> { ... }
}

/// Runner-side entry point. brokkr-sandboxd's main() calls this.
pub fn run_as_runner() -> ! { ... }
```

A couple of points worth flagging:

- `Sandbox::run` is `async`, but inside it spawns a process and the actual
  child setup is synchronous Linux code. We use `tokio::process::Command`
  for spawn + reaping so we don't block the worker's runtime.
- `run_as_runner` returns `!` because it always ends in `execve` or
  `_exit`. There is no graceful unwinding in the runner.
- `SandboxError` is the public type. Internal modules use a private
  superset (with `&'static str` step names) and convert at the boundary.

---

## 5. Subsystem designs

Each subsection below states *what* the subsystem does, *why* the design
choices, *how* it's tested, and *what could go wrong*. Implementation
order is M1–M9 in §9.

### 5.1 Mount namespace + pivot_root

**What.** Inside a fresh mount namespace, build a rootfs in tmpfs, populate
it with the minimum viable Linux file tree, and `pivot_root` into it so
the host filesystem is unreachable.

**Why pivot_root, not chroot.** `chroot` can be escaped from a process
holding a reference to the original root via fchdir. `pivot_root` followed
by `umount2(MNT_DETACH)` of the old root genuinely makes the host
filesystem unreachable.

**Layout of the sandbox rootfs:**

```
/                  tmpfs, ~size=64 MiB, mode 0755
├── usr/bin        ro bind from host /usr/bin (allowlisted host paths only)
├── usr/lib        ro bind from host /usr/lib
├── lib            ro bind from host /lib  (often a symlink → /usr/lib)
├── lib64          ro bind from host /lib64
├── bin            symlink to /usr/bin
├── etc            tmpfs, populated with a minimal passwd/group/resolv
├── proc           proc fs (mounted after PID namespace)
├── sys            ro sysfs (no /sys/fs/cgroup write access)
├── dev            tmpfs, populated with /dev/null, /dev/zero, /dev/tty,
│                  /dev/urandom, /dev/random, /dev/full
├── tmp            tmpfs, ~size=256 MiB, sticky 1777
└── work           tmpfs, ~size from RootfsSpec, mode 0755 — the workdir
```

**Bind allowlist.** `RootfsSpec.ro_binds` is the *only* way host paths
enter the sandbox. Default value comes from a build-time constant in
`brokkr-worker`. The default for Phase 2 is `/usr/bin`, `/usr/lib`,
`/lib`, `/lib64`. Adding more (e.g. `/usr/include` for compilers) is a
config knob.

**Steps inside the runner:**

1. `unshare(CLONE_NEWNS)` (already done in step B of §3.3 along with the
   other namespaces).
2. `mount(NULL, "/", NULL, MS_REC|MS_PRIVATE, NULL)` — make all mount
   propagation private so we don't bleed mounts back to the host.
3. `mkdir(/tmp/brokkr-rootfs.XXXXXX)`; mount tmpfs on it.
4. For each `ro_bind`, `mount --bind` then `mount -o remount,bind,ro`.
5. Populate `/etc` minimally: a single `passwd` line (`brokkr-sandbox:x:0:0::/:/bin/false`),
   `group` similar, `resolv.conf` empty.
6. Mount `proc` and `sysfs`.
7. Populate `/dev` from a fixed list (using `mknod` for devices is blocked
   under user namespaces, so we instead bind-mount the host's `/dev/null`
   etc. with read-write or read-only semantics as appropriate).
8. `chdir(new_root)`; `pivot_root(".", "old_root")`; `chdir("/")`;
   `umount2("/old_root", MNT_DETACH)`; `rmdir("/old_root")`.

**Tested by:** `evil/cat_etc_shadow.rs` (must fail), `evil/mount_proc_self.rs`
(`mount` syscall is blocked by seccomp, but defense-in-depth: even if it
weren't, there's nothing useful to mount), `good/ls_root.rs` (must show
exactly the expected entries).

**Risks:** WSL2 mount-namespace edge cases (some kernels reject
`MS_PRIVATE` propagation on `/`); the bind-mount allowlist is the
permanent attack surface for host access.

### 5.2 PID namespace + init reaper

**What.** Inside a fresh PID namespace, the runner is PID 1. The runner
forks once: the child execs the action; the runner-as-PID-1 reaps zombies.

**Why this matters.** If the action spawns children (compilers do; pytest
does; almost any non-trivial command does), those children become orphaned
when their parent exits and are inherited by PID 1. PID 1 has special
duties: it must `wait()` for them or they accumulate as zombies. If PID 1
exits first, the kernel sends SIGKILL to every other process in the
namespace — that's the signal we use to enforce "action ended".

**Reaper sketch:**

```rust
fn reap_until(target: Pid) -> Result<ExitStatus, SandboxError> {
    loop {
        let ws = waitpid(Pid::from_raw(-1), None)?;
        match ws {
            WaitStatus::Exited(p, code) if p == target =>
                return Ok(ExitStatus::Exited(code)),
            WaitStatus::Signaled(p, sig, dumped) if p == target =>
                return Ok(ExitStatus::Signaled { signal: sig as i32, core_dumped: dumped }),
            // Anything else is an orphaned child; just keep reaping.
            _ => continue,
        }
    }
}
```

**Edge case: orphans of the action that are still alive when the action
exits.** When the action's main process dies, the kernel re-parents its
descendants to PID 1 (the reaper). PID 1's exit (when we return from
`reap_until`) then kills them all via SIGKILL because we're the last
process in the PID namespace. This is the right behavior: actions don't
leave background processes behind.

**Tested by:** `evil/fork_bomb.rs` (paired with `pids.max=64`),
`evil/orphaned_child.rs` (parent exits, child kept running — should be
killed when sandbox ends).

### 5.3 User namespace

**What.** Map the host UID Brokkr runs as → UID 0 inside the sandbox.
Map a range of subordinate UIDs (e.g. 1–65535) → 1–65535 inside, so
multi-user setups (rare in build sandboxes) still function.

**Why.** Two reasons. First, defense in depth: even if the action escapes
all other layers, a process that's UID 0 *only inside the namespace* is
unprivileged on the host. Second, ergonomics: many tools assume root-ish
privileges (`apt`, `useradd`, `mount`) and refuse to run otherwise; user
namespaces give us "fake root" cheaply.

**Mechanics.** Two files:

- `/proc/<pid>/uid_map`: lines `<inside-id> <outside-id> <count>`.
- `/proc/<pid>/gid_map`: same format.

`uid_map` requires CAP_SETUID in the user namespace *or* a single mapping
of length 1 from the writer's own UID. We use the unprivileged form:

```
0 1000 1
```

(Where 1000 is the worker's UID.) For a range, we'd need
`newuidmap`/`newgidmap` SUID binaries, which require entries in
`/etc/subuid`. Phase 2 starts with the single-mapping form because it's
zero-config; multi-UID support is a follow-up if/when we hit a tool that
actually needs more than one inside-namespace user.

A nuance: `gid_map` cannot be written until `/proc/<pid>/setgroups`
contains the literal string `deny`. Easy to forget; covered in tests.

**Sequencing.** The new user namespace must be set up *before* anything
that requires capabilities (mounting, setting hostname). The runner
unshares NEWUSER first, then writes uid_map / setgroups / gid_map, then
unshares the rest.

**Tested by:** `evil/setuid_to_root.rs` (must not gain real root),
`good/i_am_root_inside.rs` (UID inside == 0).

**Risks:** Some hosts disable unprivileged user namespaces
(`/proc/sys/kernel/unprivileged_userns_clone=0`). On those hosts the
sandbox cannot start; we surface a clear `SandboxError::Unsupported`.
Tracked as an environment requirement in the worker's startup checks.

### 5.4 Network namespace

**What.** A fresh network namespace with no interfaces by default. Inside,
no DNS, no internet, no localhost. With `NetworkPolicy::Loopback`, bring
`lo` up.

**Why no loopback by default.** Many builds expect to be hermetic. Tools
that try to phone home (telemetry, license checks, package fetches)
should fail loudly, not silently succeed.

**Mechanics.**

- `unshare(CLONE_NEWNET)` on its own gives a netns with no interfaces.
- Loopback: open a raw netlink socket, send an `RTM_NEWLINK` message to
  bring `lo` up (or shell out to `ip link set lo up` from inside, but we
  prefer no shell-outs in the runner).

A pure-Rust netlink for "bring lo up" is ~30 lines; we'll use the
`rtnetlink` crate if its dep tree is small enough, otherwise hand-roll.

**Tested by:** `evil/curl_internet.rs` (no DNS, no route — must fail),
`good/lo_up_when_requested.rs`.

### 5.5 cgroups v2

**What.** Per-action cgroup with bounded CPU, memory, pid count, IO. Read
back accounting when the action exits.

**Why v2 only.** v1 is deprecated upstream. WSL2 and modern Ubuntu both
ship v2 unified hierarchy. Supporting v1 doubles the code and we'd be
fighting a moving target.

**Layout.**

```
/sys/fs/cgroup/brokkr.slice/                   (delegated to brokkr at startup)
└── action-<uuid>/                             (one per running action)
    ├── cgroup.subtree_control                 enables cpu, memory, pids, io
    ├── cpu.max          e.g. "200000 100000"  (200% of one CPU)
    ├── memory.max       e.g. "4294967296"     (4 GiB)
    ├── memory.swap.max  "0"                   no swap by default
    ├── pids.max         e.g. "1024"
    ├── io.max           per-device limits if requested
    └── cgroup.procs     <runner pid>
```

**Delegation.** The worker process needs write access to its slice. Two
realistic paths:

1. Run the worker under a systemd unit with `Delegate=yes`. systemd creates
   `system.slice/brokkr-worker.service/` and chowns it to the unit's user.
2. A privileged setup step (run-once on host install) creates
   `/sys/fs/cgroup/brokkr.slice/` and `chown -R brokkr:brokkr` it.

Phase 2 supports both; the worker's startup probe finds whichever is
present and errors out with a clear message if neither is.

**Accounting readback.**

- `cpu.stat` → `usage_usec`, `user_usec`, `system_usec`.
- `memory.peak` (kernel ≥ 5.19) → peak RSS. Fallback to scanning
  `memory.events`/`memory.high_max` on older kernels.
- `io.stat` → per-device read/write bytes; sum.
- `pids.peak` (kernel ≥ 5.19) → max concurrent PIDs.

OOM detection: read `memory.events` after the action exits. If
`oom_kill > 0`, the action got OOM-killed; report `ExitStatus::OutOfMemory`
regardless of the wait4 exit code.

**Tested by:** `evil/allocate_100gb.rs` (must be killed by memory cgroup
and report OutOfMemory), `evil/burn_cpu.rs` paired with cpu.max
(execution time bounded), `evil/io_write_loop.rs` paired with io.max.

**Risks:** WSL2's cgroup v2 support is recent and a few accounting
counters are still missing. We feature-detect each counter and report
"unknown" rather than failing.

### 5.6 seccomp-bpf

**What.** A default-deny syscall filter; allow only the syscalls real
build tools need. Block everything else with `SCMP_ACT_ERRNO(EPERM)` (not
`KILL` — `KILL` makes debugging painful and prevents the process from
returning a sensible error).

**Why a default-deny list, not a default-allow blocklist.** A blocklist
loses to every new syscall added to the kernel. An allowlist forces a
human review when a tool needs something we haven't seen. We can grow
the allowlist as real workloads request it.

**Implementation.** Use the `seccompiler` crate (pure Rust, MIT/Apache,
~1k stars, recent commits). Generates a BPF program from a
`SeccompFilter` and installs it with `prctl(PR_SET_SECCOMP)`. The plan's
hard rule about new dependencies is satisfied: `seccompiler` is one crate
we depend on instead of vendoring the BPF generator.

**Default allowlist.** Mirrors `docs/plan.md` §14.6, with iteration. The
list lives in `runner/seccomp.rs` as a single `&[&str]` constant; we
prefer source-as-truth over a YAML file.

```rust
const DEFAULT_ALLOW: &[&str] = &[
    "read", "write", "readv", "writev", "pread64", "pwrite64",
    "open", "openat", "openat2", "close", "close_range",
    "stat", "fstat", "lstat", "newfstatat", "statx", "lseek",
    "mmap", "mmap2", "munmap", "mremap", "mprotect", "madvise", "msync", "brk",
    "execve", "execveat",
    "wait4", "waitid", "exit", "exit_group",
    "rt_sigaction", "rt_sigprocmask", "rt_sigreturn", "rt_sigsuspend",
    "sigaltstack",
    "clone", "clone3", "fork", "vfork",  // fork/vfork still useful for spawn helpers
    "pipe", "pipe2", "dup", "dup2", "dup3",
    "getpid", "getppid", "gettid", "getuid", "geteuid", "getgid", "getegid",
    "getgroups", "setgroups",
    "getcwd", "chdir", "fchdir",
    "fcntl", "fcntl64", "ioctl",  // ioctl filtered further by arg
    "prlimit64", "getrlimit", "setrlimit",
    "arch_prctl", "prctl",        // prctl filtered further by arg
    "sched_yield", "sched_getaffinity",
    "nanosleep", "clock_nanosleep", "clock_gettime", "clock_getres",
    "futex", "futex_waitv", "set_robust_list", "get_robust_list",
    "epoll_create", "epoll_create1", "epoll_ctl", "epoll_wait", "epoll_pwait",
    "poll", "ppoll", "select", "pselect6",
    "socket", "socketpair",       // intentionally allowed; netns blocks egress
    "uname", "sysinfo",
    "getrandom",
    // …iterate from real workloads; every addition needs a one-line
    //   comment in the source explaining why.
];
```

`ioctl` and `prctl` are powerful — we filter by argument:
`prctl(PR_SET_DUMPABLE, ...)` is fine; `prctl(PR_CAPBSET_DROP, ...)` is
fine; `prctl(PR_SET_KEEPCAPS, ...)` is denied. Argument filtering is
where seccomp's complexity lives; we'll start with a small set and grow.

**Tested by:** `evil/mount_syscall.rs`, `evil/ptrace_self.rs`,
`evil/keyctl.rs`, `evil/io_uring_setup.rs` (must all return EPERM).

### 5.7 Capability dropping

**What.** Drop every Linux capability from the bounding set, the inheritable
set, the permitted set, and the effective set, except for any explicitly
listed in `SandboxConfig.retained_caps`. Phase 2 default: empty.

**How.** `prctl(PR_CAPBSET_DROP, cap)` for each capability 0..63. Then
`capset` to clear the per-thread sets. Then `prctl(PR_SET_NO_NEW_PRIVS, 1)`
so no future `execve` of a setuid binary can re-acquire privilege.

**Tested by:** `evil/setuid_binary.rs` (run a setuid-root binary inside,
check its EUID stays at 0-of-userns/non-0-of-host),
`evil/cap_get_proc.rs`.

### 5.8 Determinism guards

**What.** Prevent the action from reading ambient inputs that vary across
runs.

- Hostname → fixed (`brokkr-sandbox`).
- Timezone → UTC (`/etc/localtime` symlinked to `Etc/UTC`).
- `LD_PRELOAD`, `LD_LIBRARY_PATH` → stripped.
- `PATH` → replaced with `/usr/bin:/bin` (configurable).
- `HOME` → `/work`.
- `SOURCE_DATE_EPOCH` → set if requested (lets reproducible-build tools
  produce identical archives).
- `/proc/self/environ` is whatever we passed.

**Why this is a sandbox concern, not a worker concern.** The worker
already controls the env it passes; but env stripping and `LD_PRELOAD`
blocking must happen *before* `execve` of the action, not before
`execve` of the runner. Otherwise the runner inherits the worker's env
and could itself be compromised.

**Tested by:** `good/hostname_is_brokkr_sandbox.rs`,
`good/tz_is_utc.rs`, `evil/ld_preload_attempt.rs`.

### 5.9 Resource accounting

Already covered as a side-effect of cgroups v2 (§5.5) and
PID-namespace timing (§5.2). The worker reads counters between
"runner exits" and "tear down cgroup", populates
`SandboxOutcome.accounting`, and propagates into the REAPI
`ActionResult.execution_metadata` fields:

- `worker_start_timestamp`, `worker_completed_timestamp`
- `execution_start_timestamp`, `execution_completed_timestamp`
- `virtual_execution_duration` (when we have per-action virtualized time;
  otherwise wall-clock)

The cgroup-derived counters are also exported as a custom
`auxiliary_metadata` Any payload (proto defined in `brokkr-proto`), so
operators can graph them.

---

## 6. Wiring into brokkr-worker

`brokkr-worker/src/runner.rs` currently does:

```rust
let output = Command::new(argv0).args(argv).output().await?;
```

Phase 2 replaces this with:

```rust
let cfg = SandboxConfig::from_action(&command, &workdir, &policy);
let outcome = sandbox.run(cfg).await?;
```

Where `SandboxConfig::from_action` is a small helper in `brokkr-worker`
that translates a REAPI `Command` + worker-level `SandboxPolicy` (host
allowlist, default limits) into a fully-formed `SandboxConfig`.

A new `WorkerConfig` field `sandbox_policy: SandboxPolicy` configures
defaults:

- `default_limits: ResourceLimits` (CPU/mem/pids/io defaults).
- `default_ro_binds: Vec<(PathBuf, PathBuf)>`.
- `network: NetworkPolicy` default.
- `wall_clock_timeout: Duration`.

Per-action overrides come from the REAPI `Action.Platform` properties:
`network=loopback`, `memory_bytes=8589934592`, etc. The worker validates
overrides against a list of allowed keys.

Existing Phase 1 behavior — process spawn with no isolation — is
preserved behind a `--no-sandbox` worker flag for development on hosts
without the required kernel features (e.g. macOS, an old WSL2 kernel).
A worker with `--no-sandbox` logs a loud warning at startup and refuses
to register if the control plane has marked the cluster as "sandbox
required".

---

## 7. Configuration & CLI surface

### 7.1 Worker

```
brokkr-worker --control http://… [options]
  --no-sandbox                    Phase 1 fallback; warns at startup.
  --sandbox-rootfs-bind PATH:PATH  Add a host:sandbox bind to the default rootfs.
  --sandbox-default-mem BYTES     Default memory.max (e.g. 4G).
  --sandbox-default-cpu PERCENT   Default cpu.max (e.g. 200 = 2 cores).
  --sandbox-default-pids N
  --sandbox-cgroup-root PATH      Default /sys/fs/cgroup/brokkr.slice
  --sandbox-runner PATH           Default: discovered next to the worker binary
```

### 7.2 SDK / CLI

`brokk run` gains:

```
brokk run [--memory 4G] [--cpu 200] [--network loopback]
          [--mount HOST:SANDBOX[:ro]] [--env KEY=VAL]
          -- argv...
```

These map to `Action.Platform` properties; Phase 2 honors them.

### 7.3 Host one-time setup

A `scripts/install-cgroup-slice.sh` (run once per host) does the cgroup
delegation. A `brokkr-worker --check-host` mode exits 0 iff the host
supports everything Phase 2 needs and prints a checklist on what's
missing otherwise. This subcommand is independently useful and is its
own milestone (M1).

---

## 8. Testing strategy

### 8.1 Evil-action matrix

Each test is a small Rust binary built into a static, no-network test
helper. The test suite compiles those helpers into the action's input
tree, runs them in the sandbox, and asserts the failure mode.

| ID    | Action attempts                             | Layer that stops it       | Expected outcome                          |
|-------|---------------------------------------------|---------------------------|-------------------------------------------|
| EV-01 | `cat /etc/shadow`                           | mount-ns rootfs allowlist | exit != 0, stderr "no such file"          |
| EV-02 | `mount(...)` syscall                        | seccomp                   | EPERM                                     |
| EV-03 | `keyctl(...)` syscall                       | seccomp                   | EPERM                                     |
| EV-04 | `ptrace(SELF)`                              | seccomp                   | EPERM                                     |
| EV-05 | fork bomb                                   | pids cgroup               | killed; exit code reflects SIGKILL        |
| EV-06 | `mmap` 100 GiB                              | memory cgroup             | `ExitStatus::OutOfMemory`                 |
| EV-07 | DNS lookup                                  | netns                     | exit != 0; no network reachable           |
| EV-08 | `socket(AF_INET, …)` + connect to 1.1.1.1   | netns                     | connect ENETUNREACH                       |
| EV-09 | `RDTSC`                                     | seccomp                   | SIGSYS or EPERM (CPU dependent)           |
| EV-10 | setuid binary inside                        | NO_NEW_PRIVS              | EUID stays unprivileged                   |
| EV-11 | `LD_PRELOAD=/host/evil.so`                  | env scrubber              | env stripped; library not loaded          |
| EV-12 | `gettimeofday`-based spin loop, wall=1s     | wall-clock timeout        | `ExitStatus::Timeout`                     |
| EV-13 | leave a child running forever               | PID-ns reaper             | child SIGKILLed when parent exits         |
| EV-14 | write to `/sys/fs/cgroup/cgroup.procs`      | sysfs read-only           | EACCES                                    |
| EV-15 | recursive bind-mount escape attempt         | mount propagation private | exit != 0; no new mounts visible to host  |
| EV-16 | open `/proc/1/root/etc/shadow`              | PID-ns                    | host PID 1 not visible                    |

Each row is one `#[test]` in `crates/brokkr-sandbox/tests/evil/`. Tests
that need root or specific kernel features feature-gate via
`#[cfg_attr(not(target_os = "linux"), ignore)]` plus a runtime probe.

### 8.2 Real-world acceptance

| ID    | Action                                      | Expected                                  |
|-------|---------------------------------------------|-------------------------------------------|
| AC-01 | `gcc hello.c -o hello && ./hello`           | exit 0, "Hello, world!" on stdout         |
| AC-02 | `python3 -c "print('hi')"`                  | exit 0, "hi"                              |
| AC-03 | `cargo build` of a tiny crate (with toolchain bind-mounted) | exit 0, artifacts in /work          |
| AC-04 | `tar` + `gzip` produce byte-identical output across two runs | digests equal (determinism)        |

AC-03 is aspirational — getting cargo working inside requires careful
bind-mount work (rustc, cargo, registry cache, target dir). It's a goal,
not a blocker.

### 8.3 Unit vs integration boundary

- **Unit tests** (in `src/**/*.rs`): pure functions only — config
  validation, seccomp filter compilation, cgroup path math, etc. These
  run in seconds on any host.
- **Integration tests** (`tests/`): require Linux + namespaces + cgroups.
  Marked `#[cfg(target_os = "linux")]`; in CI they only run on the Linux
  matrix entries.
- **Soak**: a single `#[ignore]`d test runs every evil-action 100 times
  to catch flakes. Equivalent to Phase 1's soak test pattern.

---

## 9. Milestones (incremental delivery)

Each milestone is a single PR (sometimes two) that compiles, tests, and
ships independently. No milestone leaves the tree in a half-built state.

| #  | Branch                                        | Outcome                                                                     | LOC est. |
|----|-----------------------------------------------|------------------------------------------------------------------------------|----------|
| M1 | `feat/phase2-host-check`                      | `brokkr-worker --check-host` prints a kernel-feature checklist; install script | ~250 |
| M2 | `feat/phase2-sandbox-skeleton`                | `brokkr-sandbox` crate + `brokkr-sandboxd` bin compile; `Sandbox::run` does plain spawn (Phase-1 parity) under a re-exec model | ~600 |
| M3 | `feat/phase2-mount-namespace`                 | mount ns + pivot_root + bind allowlist; EV-01, EV-15 pass                   | ~700 |
| M4 | `feat/phase2-pid-user-namespaces`             | PID + user namespaces + reaper; EV-13, EV-16, AC-01 pass                    | ~500 |
| M5 | `feat/phase2-network-namespace`               | netns + lo toggle; EV-07, EV-08 pass                                         | ~250 |
| M6 | `feat/phase2-cgroups`                         | cgroups v2 + accounting readback; EV-05, EV-06 pass                          | ~600 |
| M7 | `feat/phase2-seccomp-and-caps`                | seccomp default-deny + cap dropping + NO_NEW_PRIVS; EV-02..04, EV-09, EV-10, EV-14 pass | ~500 |
| M8 | `feat/phase2-determinism-and-timeout`         | env scrubbing, hostname/TZ, wall-clock; EV-11, EV-12, AC-04 pass            | ~250 |
| M9 | `feat/phase2-worker-integration-and-defaults` | wire `brokkr-worker` to use the sandbox by default; `--no-sandbox` flag      | ~200 |

Total budget: ~3.8k lines, including tests and docs. Each milestone PR
includes its CHANGELOG entry, the relevant evil-action tests, a short
note in `docs/journal/phase-2.md` (a journal, started in M1), and any
proto changes needed for resource accounting (M6 only).

---

## 10. CI & host compatibility

### 10.1 CI matrix additions

GitHub Actions `ubuntu-22.04` and `ubuntu-24.04` are the primary CI
hosts. We need:

- `unprivileged_userns_clone=1` — default on Ubuntu since 16.04.
- cgroups v2 unified hierarchy — default on Ubuntu since 21.10.
- A writable cgroup slice for the runner UID.

CI step before tests: `scripts/install-cgroup-slice.sh` runs once and
chowns `/sys/fs/cgroup/brokkr.slice` to the actions runner user. This is
sudo-without-prompt on the GitHub runners.

`aarch64` is already in the Phase 0 CI matrix; seccomp filters are
arch-specific (syscall numbers differ); we generate the filter from a
single source in `seccomp.rs` parameterized by `std::env::consts::ARCH`
at runtime. Tests run on both arches.

### 10.2 Local-dev hosts

| Host                      | Status           |
|---------------------------|------------------|
| Ubuntu 22.04+             | full support     |
| Debian 12+                | full support     |
| WSL2 (kernel ≥ 5.15)      | full support — verified manually; cgroup probe must succeed |
| Fedora 38+                | full support     |
| macOS                     | not supported; worker runs with `--no-sandbox` only |
| Windows native            | not supported    |

### 10.3 `--check-host` output

```
$ brokkr-worker --check-host
brokkr-worker host compatibility check (linux x86_64, kernel 6.6.87)

[ OK   ] kernel ≥ 5.10
[ OK   ] unprivileged user namespaces enabled
[ OK   ] cgroup v2 unified hierarchy
[ OK   ] /sys/fs/cgroup/brokkr.slice writable as uid 1000
[ OK   ] seccomp-bpf available
[ WARN ] memory.peak not present (kernel < 5.19); falling back to memory.events
[ OK   ] /proc/self/setgroups present

Sandbox is functional. 1 warning.
```

Exit code: 0 if functional (warnings allowed), 1 otherwise.

---

## 11. Risks & open questions

| Risk                                                                 | Mitigation                                                              |
|----------------------------------------------------------------------|-------------------------------------------------------------------------|
| WSL2 cgroup quirks                                                   | feature-detect each counter; `--check-host` surfaces gaps               |
| Hosts with `unprivileged_userns_clone=0`                             | refuse to start; recommend running worker as a user in `/etc/subuid`    |
| Subtle seccomp gaps that let an action `clone3(CLONE_NEWUSER)` and re-escalate | filter `clone`/`clone3` flags; test EV-17 (TBD)            |
| Output materialization races (action writes after exit)              | cgroup freeze before harvest                                            |
| Per-arch syscall numbers (x86_64 vs aarch64)                         | seccompiler handles this; CI tests both                                 |
| Performance regression vs Phase-1 (sandbox setup ~10–30 ms per action) | acceptable for Phase 2; revisit with a runner-pool in Phase 4           |

### Open questions to resolve in M1

1. **Cgroup delegation default.** systemd unit + `Delegate=yes`, or a
   manual `chown` script? Pick one for the docs; both stay supported.
2. **`brokkr-sandboxd` discovery.** Hard-coded relative path next to the
   worker, or `$PATH` lookup? Probably "relative path with PATH fallback".
3. **Where does `/work`'s contents come from?** Phase 2 copies
   action-input bytes from local CAS into a tmpfs `/work`. Phase 3
   replaces that with FUSE; the API stays the same.
4. **Partial outputs on OOM.** If the action gets killed mid-write,
   should we still upload `/work/*`? Decision: yes, but flag the
   `ActionResult` with a status code so the client knows it's incomplete.
5. **systemd vs no-systemd hosts.** WSL2 may or may not have systemd. We
   support both; the cgroup setup path branches at startup.

---

## 12. Definition of done

Phase 2 is done when, on a clean Ubuntu 24.04 host:

1. Every evil-action test (§8.1, EV-01..16) passes deterministically 100
   times in a row.
2. AC-01 (`gcc hello.c`) passes deterministically.
3. AC-04 (tar+gzip determinism) produces byte-identical outputs across
   two runs of the same action.
4. `brokkr-worker --check-host` exits 0.
5. The end-to-end Phase 1 test (`echo hello world` + cache hit) still
   passes — Phase 2 is a strict superset.
6. `cargo clippy --workspace --all-targets -- -D warnings` is clean.
7. `cargo test --workspace` is green; `cargo test --workspace -- --ignored`
   (the soak) is green.
8. `docs/journal/phase-2.md` retrospective is written.

---

## 13. Out of scope (deferred)

The following are tempting to include but stay out of Phase 2:

- **Snapshotting / runner pools.** Sandbox setup is a few ms; we'll
  revisit if profiling says it matters.
- **Image-based rootfs (OCI / podman images).** Bind-mount allowlist is
  enough until users ask for "I want this random Docker image to be my
  rootfs". When they do, that's a phase of its own.
- **Per-action overlay-fs writable layers over a frozen base.** The
  tmpfs-everywhere approach is simpler and fast; overlay-fs is a Phase 3+
  optimization.
- **`gVisor`-style userspace kernel.** Out of scope by `CLAUDE.md` rule #9.
- **Parallel actions on one worker.** Phase 2 assumes one action at a
  time. Concurrency is Phase 4 alongside scheduling.
- **Running actions as non-root inside the sandbox.** Default is UID 0
  inside (mapped to unprivileged outside). Letting actions run as a
  non-zero inside-UID is a small follow-up, not a blocker.

---

## Appendix A — Why each kernel facility

A short justification table for the reviewer who asks "do we really need
*all* of these?":

| Facility         | What we'd lose without it                                                            |
|------------------|--------------------------------------------------------------------------------------|
| mount ns         | Action could read every host file the worker UID can read                            |
| user ns          | Action runs as the worker's real UID; any escape is a real-UID escape                |
| pid ns           | Action sees and can signal host processes; PID-1 reaper has nothing to reap          |
| net ns           | Action has full network access and can exfiltrate or phone home                      |
| uts/ipc ns       | Action sees host hostname and POSIX IPC objects; minor leakage but cheap to close    |
| cgroup ns        | Action sees host's cgroup hierarchy at `/proc/self/cgroup`; mostly cosmetic          |
| cgroup v2 limits | Action can fork-bomb, OOM the host, saturate disks                                    |
| seccomp          | Action can call any syscall the kernel exposes — incl. ones with known CVEs in older kernels |
| caps drop        | Setuid binaries inside escalate; ambient privileges leak                             |
| no_new_privs     | Setuid still works after our drop; one execve = full privilege re-acquisition        |
| determinism      | Cache hits diverge from cache misses; outputs vary across runs                       |

We turn all of them on; missing any one of them changes the threat
model.

---

## Appendix B — A worked example

What `brokk run -- gcc hello.c -o hello` looks like on a Phase-2 worker:

```
[trace] client::execute action_digest=…/254 cache_hit=miss
[trace] control::dispatch action_digest=…/254 job_id=…
[info ] worker received job, allocating sandbox
[trace] sandbox::host::cgroup created /sys/fs/cgroup/brokkr.slice/action-…
[trace] sandbox::host::workspace staged input root (1 file: hello.c, 124 B)
[debug] sandbox::host spawned brokkr-sandboxd pid=…
[debug] sandbox::runner unshared NEWUSER|NEWNS|NEWPID|NEWNET|NEWUTS|NEWIPC|NEWCGROUP
[debug] sandbox::runner uid_map "0 1000 1" written
[debug] sandbox::runner pivot_root into /tmp/brokkr-rootfs.…
[debug] sandbox::runner mounted /usr/bin (ro), /usr/lib (ro), /lib (ro), /lib64 (ro)
[debug] sandbox::runner mounted /proc, /sys (ro), /dev (minimal), /tmp (256 MiB), /work (1 GiB)
[debug] sandbox::runner hostname=brokkr-sandbox tz=UTC PATH=/usr/bin:/bin
[debug] sandbox::runner dropped 38 capabilities, NO_NEW_PRIVS=1
[debug] sandbox::runner installed seccomp filter (allowed: 71 syscalls)
[debug] sandbox::runner forked pid=2 ./gcc hello.c -o hello
[debug] sandbox::runner reaped pid=2 status=Exited(0)
[trace] sandbox::host runner exited; harvesting cgroup counters
[debug] sandbox::host cpu_user=120ms cpu_sys=45ms mem_peak=42 MiB pids_peak=12
[trace] worker::run_action exit_code=0 stdout=0 B stderr=0 B
[trace] cas upload outputs (1 file: hello, 16 KiB)
[info ] action complete
```

That's Phase 2.
